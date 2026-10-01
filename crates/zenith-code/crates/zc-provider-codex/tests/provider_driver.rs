//! `CodexProviderDriver` on the provider core's driver SPI: config decoding, instance creation
//! against missing and fake `codex` executables, scope teardown. Never runs the real `codex` nor
//! reads the real `~/.codex` (`CODEX_HOME` and `homePath` point at temp dirs).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use zc_contracts::{
    ApprovalRequestId, ProviderApprovalDecision, ProviderDriverKind, ProviderEvent, ProviderInstanceId, ProviderSession, ProviderSessionStartInput,
    ProviderSessionStatus, ProviderTurnStartResult, ProviderUserInputAnswers, ServerProviderAuthStatus, ServerProviderState, TurnId,
};
use zc_core::PubSub;
use zc_ports::contracts::{ServerSettings, ServerSettingsError, ServerSettingsPatch};
use zc_ports::{EventStream, SettingsService};
use zc_provider_codex::adapter::RuntimeFactory;
use zc_provider_codex::errors::CodexSessionRuntimeError;
use zc_provider_codex::session_runtime::{CodexRuntime, CodexSessionRuntimeOptions, SendTurnInput};
use zc_provider_codex::thread_history::CodexThreadSnapshot;
use zc_provider_codex::CodexProviderDriver;
use zc_providers::logger::EventNdjsonLogStoreOptions;
use zc_providers::{Driver, DriverCreateInput, DriverEnv, InstanceScope, ModelManifest, ProviderEventLoggers};

struct Settings(PubSub<ServerSettings>);

fn server_settings() -> ServerSettings {
    serde_json::from_value(json!({})).expect("default server settings")
}

#[async_trait]
impl SettingsService for Settings {
    async fn get_settings(&self) -> Result<ServerSettings, ServerSettingsError> {
        Ok(server_settings())
    }
    async fn update_settings(&self, _patch: ServerSettingsPatch) -> Result<ServerSettings, ServerSettingsError> {
        Ok(server_settings())
    }
    fn subscribe_changes(&self) -> EventStream<ServerSettings> {
        self.0.subscribe().boxed()
    }
}

struct Fixture {
    root: tempfile::TempDir,
    env: DriverEnv,
}

impl Fixture {
    fn new() -> Self {
        Self::with_loggers(|_| ProviderEventLoggers::none())
    }

    fn with_loggers(loggers: impl FnOnce(&Path) -> ProviderEventLoggers) -> Self {
        let root = tempfile::tempdir().unwrap();
        let codex_home = root.path().join("codex-home");
        std::fs::create_dir_all(&codex_home).unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let base_env = HashMap::from([
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ("CODEX_HOME".to_owned(), codex_home.to_string_lossy().into_owned()),
        ]);
        let env = DriverEnv {
            event_loggers: loggers(root.path()),
            model_manifest: ModelManifest::bundled_only(),
            settings: Arc::new(Settings(PubSub::new())),
            background_policy: None,
            server_cwd: workspace,
            state_dir: root.path().join("userdata"),
            attachments_dir: root.path().join("userdata").join("attachments"),
            base_env,
            mcp_sessions: None,
        };
        Self { root, env }
    }

    fn codex_home(&self) -> PathBuf {
        self.root.path().join("codex-home")
    }

    fn config(&self, binary_path: &str) -> Value {
        json!({"binaryPath": binary_path, "homePath": self.codex_home().to_string_lossy()})
    }

    fn input(&self, instance_id: &str, enabled: bool, config: Value) -> (DriverCreateInput, InstanceScope) {
        let scope = InstanceScope::new();
        (
            DriverCreateInput {
                instance_id: ProviderInstanceId::new(instance_id),
                display_name: Some("Codex Studio".into()),
                accent_color: Some("#336699".into()),
                environment: Vec::new(),
                enabled,
                config,
                scope: scope.clone(),
            },
            scope,
        )
    }

    /// A `codex` that exits at once (never answers the handshake).
    #[cfg(unix)]
    fn failing_codex(&self) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = self.root.path().join("bin").join("codex");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "#!/bin/sh\nexit 3\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }
}

#[test]
fn decodes_config_with_schema_defaults() {
    let fixture = Fixture::new();
    let driver = CodexProviderDriver::new(fixture.env.clone());
    assert_eq!(driver.driver_kind(), ProviderDriverKind::new("codex"));
    assert_eq!(driver.metadata().display_name, "Codex");
    assert!(driver.metadata().supports_multiple_instances);

    let defaults = driver.default_config();
    assert_eq!(defaults, driver.decode_config(&json!({})).unwrap());
    assert_eq!(defaults["enabled"], true);
    assert_eq!(defaults["binaryPath"], "codex");
    assert_eq!(defaults["homePath"], "");
    assert_eq!(defaults["shadowHomePath"], "");
    assert_eq!(defaults["customModels"], json!([]));

    let decoded = driver
        .decode_config(&json!({"binaryPath": "/opt/made-up/codex", "launchArgs": "--flag", "setupMode": "existing"}))
        .unwrap();
    assert_eq!(decoded["binaryPath"], "/opt/made-up/codex");
    assert_eq!(decoded["launchArgs"], "--flag");
    assert_eq!(decoded["setupMode"], "existing");

    assert!(driver.decode_config(&json!("codex")).is_err());
    assert!(driver.decode_config(&json!(42)).is_err());
    assert!(driver.decode_config(&json!({"setupMode": "sideways"})).is_err());
}

#[tokio::test]
async fn a_missing_binary_is_an_error_snapshot_not_a_failure() {
    let fixture = Fixture::new();
    let driver = CodexProviderDriver::new(fixture.env.clone());
    let missing = fixture.root.path().join("nowhere").join("codex");
    let (input, scope) = fixture.input("codex_studio", true, driver.decode_config(&fixture.config(&missing.to_string_lossy())).unwrap());
    let instance = driver.create(input).await.expect("instance");

    assert_eq!(instance.instance_id, ProviderInstanceId::new("codex_studio"));
    assert_eq!(instance.driver_kind, ProviderDriverKind::new("codex"));
    assert!(instance.enabled);
    assert_eq!(
        instance.continuation_identity.continuation_key,
        format!("codex:home:{}", fixture.codex_home().display())
    );
    assert!(instance.text_generation.is_none());
    assert!(instance.consume_reset_credit.is_some());
    assert_eq!(instance.adapter.provider(), ProviderDriverKind::new("codex"));

    let pending = instance.snapshot.get_snapshot().await;
    assert_eq!(pending.driver, ProviderDriverKind::new("codex"));
    assert_eq!(pending.instance_id, ProviderInstanceId::new("codex_studio"));
    assert_eq!(pending.display_name.as_deref(), Some("Codex Studio"));
    assert_eq!(pending.accent_color.as_deref(), Some("#336699"));
    assert_eq!(
        pending.continuation.as_ref().map(|continuation| continuation.group_key.clone()),
        Some(format!("codex:home:{}", fixture.codex_home().display()))
    );

    let checked = tokio::time::timeout(Duration::from_secs(15), instance.snapshot.refresh())
        .await
        .expect("probe finishes");
    assert_eq!(checked.instance_id, ProviderInstanceId::new("codex_studio"));
    assert_eq!(checked.driver, ProviderDriverKind::new("codex"));
    assert!(!checked.installed);
    assert_eq!(checked.status, ServerProviderState::Error);
    assert_eq!(checked.auth.status, ServerProviderAuthStatus::Unknown);
    assert!(
        checked
            .message
            .as_deref()
            .is_some_and(|message| message.starts_with("Could not start Codex CLI")),
        "{:?}",
        checked.message
    );

    let maintenance = instance.snapshot.resolve_maintenance(false).await;
    assert_eq!(maintenance.provider, ProviderDriverKind::new("codex"));
    assert_eq!(maintenance.package_name.as_deref(), Some("@openai/codex"));
    assert!(maintenance.update.is_none());

    // The skills probe fails like the status probe, as an error rather than a panic.
    let skills = (instance.snapshot_for_cwd.as_ref().unwrap())(fixture.env.server_cwd.to_string_lossy().into_owned()).await;
    assert!(skills.is_err());

    scope.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn a_codex_that_exits_is_installed_but_failing() {
    let fixture = Fixture::new();
    let driver = CodexProviderDriver::new(fixture.env.clone());
    let codex = fixture.failing_codex();
    let (input, scope) = fixture.input("codex_exits", true, driver.decode_config(&fixture.config(&codex)).unwrap());
    let instance = driver.create(input).await.expect("instance");

    let checked = tokio::time::timeout(Duration::from_secs(15), instance.snapshot.refresh())
        .await
        .expect("probe finishes");
    assert_eq!(checked.instance_id, ProviderInstanceId::new("codex_exits"));
    assert!(checked.installed);
    assert_eq!(checked.status, ServerProviderState::Error);
    assert!(
        checked
            .message
            .as_deref()
            .is_some_and(|message| message.starts_with("Codex app-server provider probe failed")),
        "{:?}",
        checked.message
    );
    scope.close().await;
}

#[tokio::test]
async fn disabled_instances_report_disabled_and_skip_probes() {
    let fixture = Fixture::new();
    let driver = CodexProviderDriver::new(fixture.env.clone());
    let missing = fixture.root.path().join("nowhere").join("codex");
    let (input, scope) = fixture.input("codex_off", false, driver.decode_config(&fixture.config(&missing.to_string_lossy())).unwrap());
    let instance = driver.create(input).await.expect("instance");
    assert!(!instance.enabled);

    let checked = instance.snapshot.refresh().await;
    assert!(!checked.enabled);
    assert_eq!(checked.status, ServerProviderState::Disabled);
    assert_eq!(checked.message.as_deref(), Some("Codex is disabled in zenith code settings."));

    // No skills probe: the machine snapshot comes back as is.
    let scoped = (instance.snapshot_for_cwd.as_ref().unwrap())("/made-up/workspace".into()).await.unwrap();
    assert_eq!(scoped.instance_id, ProviderInstanceId::new("codex_off"));
    assert_eq!(scoped.status, ServerProviderState::Disabled);
    scope.close().await;
}

#[tokio::test]
async fn managed_mode_without_setup_asks_to_set_up_codex() {
    let fixture = Fixture::new();
    let driver = CodexProviderDriver::new(fixture.env.clone());
    let config = driver.decode_config(&json!({"setupMode": "managed"})).unwrap();
    let (input, scope) = fixture.input("codex_managed", true, config);
    let instance = driver.create(input).await.expect("instance");
    assert!(instance.consume_reset_credit.is_none());

    let checked = instance.snapshot.refresh().await;
    assert_eq!(checked.instance_id, ProviderInstanceId::new("codex_managed"));
    assert!(!checked.installed);
    assert_eq!(checked.message.as_deref(), Some("Set up Codex to get started."));
    assert_eq!(checked.auth.status, ServerProviderAuthStatus::Unauthenticated);
    assert!(checked.models.is_empty());
    let maintenance = instance.snapshot.resolve_maintenance(true).await;
    assert!(maintenance.package_name.is_none() && maintenance.update.is_none());

    // Sessions are refused until managed Codex is set up.
    let started = instance
        .adapter
        .start_session(serde_json::from_value::<ProviderSessionStartInput>(json!({"threadId": "thread-managed", "runtimeMode": "full-access"})).unwrap())
        .await;
    assert!(started.is_err());
    scope.close().await;
}

// ---------------------------------------------------------------------------------------------
// Scope teardown, with fake session runtimes

struct FakeRuntime {
    options: CodexSessionRuntimeOptions,
    sender: Mutex<Option<mpsc::UnboundedSender<ProviderEvent>>>,
    receiver: Mutex<Option<mpsc::UnboundedReceiver<ProviderEvent>>>,
    closes: Arc<Mutex<Vec<String>>>,
}

impl FakeRuntime {
    fn session(&self) -> ProviderSession {
        ProviderSession {
            provider: ProviderDriverKind::new("codex"),
            provider_instance_id: None,
            status: ProviderSessionStatus::Ready,
            runtime_mode: self.options.runtime_mode,
            cwd: Some(self.options.cwd.clone()),
            model: None,
            thread_id: self.options.thread_id.clone(),
            resume_cursor: None,
            active_turn_id: None,
            created_at: "2026-01-01T00:00:00.000Z".into(),
            updated_at: "2026-01-01T00:00:00.000Z".into(),
            last_error: None,
        }
    }

    fn emit(&self, event: Value) {
        let event: ProviderEvent = serde_json::from_value(event).unwrap();
        if let Some(sender) = self.sender.lock().unwrap().as_ref() {
            sender.send(event).unwrap();
        }
    }
}

#[async_trait]
impl CodexRuntime for FakeRuntime {
    async fn start(&self) -> Result<ProviderSession, CodexSessionRuntimeError> {
        Ok(self.session())
    }
    async fn get_session(&self) -> ProviderSession {
        self.session()
    }
    async fn send_turn(&self, _input: SendTurnInput) -> Result<ProviderTurnStartResult, CodexSessionRuntimeError> {
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
            thread_id: "provider-thread".into(),
            turns: Vec::new(),
        })
    }
    async fn rollback_thread(&self, _num_turns: usize) -> Result<CodexThreadSnapshot, CodexSessionRuntimeError> {
        self.read_thread().await
    }
    async fn upload_feedback(&self, _reason: Option<String>) -> Result<String, CodexSessionRuntimeError> {
        Ok("provider-thread".into())
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
        self.closes.lock().unwrap().push(self.options.thread_id.as_str().to_owned());
        self.sender.lock().unwrap().take();
    }
}

#[tokio::test]
async fn closing_the_scope_stops_every_session_and_native_events_are_logged() {
    let fixture =
        Fixture::with_loggers(|root| ProviderEventLoggers::open(&root.join("logs").join("provider").join("events.log"), EventNdjsonLogStoreOptions::default()));
    let runtimes: Arc<Mutex<Vec<Arc<FakeRuntime>>>> = Arc::new(Mutex::new(Vec::new()));
    let closes = Arc::new(Mutex::new(Vec::new()));
    let factory: RuntimeFactory = {
        let runtimes = runtimes.clone();
        let closes = closes.clone();
        Arc::new(move |options| {
            let (sender, receiver) = mpsc::unbounded_channel();
            let runtime = Arc::new(FakeRuntime {
                options,
                sender: Mutex::new(Some(sender)),
                receiver: Mutex::new(Some(receiver)),
                closes: closes.clone(),
            });
            runtimes.lock().unwrap().push(runtime.clone());
            Box::pin(async move { Ok(runtime as Arc<dyn CodexRuntime>) })
        })
    };
    let driver = CodexProviderDriver::new(fixture.env.clone()).with_runtime_factory(Some(factory));
    let missing = fixture.root.path().join("nowhere").join("codex");
    let (input, scope) = fixture.input(
        "codex_teardown",
        true,
        driver.decode_config(&fixture.config(&missing.to_string_lossy())).unwrap(),
    );
    let instance = driver.create(input).await.expect("instance");
    let mut changes = instance.snapshot.subscribe_changes();

    for thread in ["thread-alpha", "thread-beta"] {
        let start: ProviderSessionStartInput = serde_json::from_value(json!({"threadId": thread, "runtimeMode": "full-access"})).unwrap();
        instance.adapter.start_session(start).await.expect("session starts");
    }
    {
        let runtimes = runtimes.lock().unwrap();
        assert_eq!(runtimes.len(), 2);
        // Sessions without a cwd run in the server cwd, with the instance's environment.
        assert_eq!(runtimes[0].options.cwd, fixture.env.server_cwd.to_string_lossy());
        assert_eq!(runtimes[0].options.provider_instance_id, Some(ProviderInstanceId::new("codex_teardown")));
        assert!(runtimes[0].options.models.is_some());
        assert_eq!(
            runtimes[0]
                .options
                .environment
                .as_ref()
                .and_then(|env| env.get("CODEX_HOME"))
                .map(String::as_str),
            Some(fixture.codex_home().to_string_lossy().as_ref())
        );
        runtimes[0].emit(json!({
            "id": "evt-native-1",
            "kind": "notification",
            "provider": "codex",
            "threadId": "thread-alpha",
            "createdAt": "2026-01-01T00:00:00.000Z",
            "method": "thread/started",
            "payload": {"thread": {"id": "provider-thread"}}
        }));
    }
    assert_eq!(instance.adapter.list_sessions().await.len(), 2);
    tokio::time::sleep(Duration::from_millis(50)).await;

    scope.close().await;

    let mut closed = closes.lock().unwrap().clone();
    closed.sort();
    assert_eq!(closed, vec!["thread-alpha", "thread-beta"]);
    assert!(instance.adapter.list_sessions().await.is_empty());
    // The snapshot's change stream ends with the scope.
    let ended = tokio::time::timeout(Duration::from_secs(5), async { while changes.next().await.is_some() {} }).await;
    assert!(ended.is_ok(), "the snapshot stream ends when the scope closes");

    let store = fixture.env.event_loggers.store().expect("log store opened");
    store.flush();
    let logs = std::fs::read_dir(store.file_path().parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| std::fs::read_to_string(entry.path()).unwrap_or_default())
        .collect::<String>();
    assert!(logs.contains("NTIVE:"), "{logs}");
    assert!(logs.contains("thread/started"), "{logs}");
    assert!(!logs.contains("CANON:"), "canonical lines are the provider service's");
    fixture.env.event_loggers.close();
}
