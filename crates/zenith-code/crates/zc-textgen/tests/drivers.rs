//! The driver wrapper end to end: instances built by the provider instance registry get the
//! text generator their driver kind has in TS, and the service routes to it by instance id.

mod support;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::{json, Value};
use support::*;
use zc_contracts::{
    ApprovalRequestId, ProviderApprovalDecision, ProviderDriverKind, ProviderInstanceConfig, ProviderInstanceId, ProviderRuntimeEvent, ProviderSendTurnInput,
    ProviderSession, ProviderSessionStartInput, ProviderTurnStartResult, ProviderUsageLimitsUpdate, ProviderUserInputAnswers, ServerProvider, ThreadId, TurnId,
};
use zc_ports::adapter::{AdapterCapabilities, AdapterResult, ProviderAdapter, SessionModelSwitch, ThreadSnapshot};
use zc_ports::contracts::{ServerSettings, ServerSettingsError, ServerSettingsPatch};
use zc_ports::{EventStream, SettingsService, TextGeneration};
use zc_providers::driver::{ContinuationIdentity, ServerProviderSource};
use zc_providers::snapshot::ProviderMaintenanceCapabilities;
use zc_providers::{
    Driver, DriverCreateInput, DriverEnv, DriverMetadata, ModelManifest, ProviderDriverError, ProviderEventLoggers, ProviderInstance, ProviderInstanceRegistry,
};
use zc_textgen::{with_text_generation, TextGenerationService};

struct NoSettings;

#[async_trait]
impl SettingsService for NoSettings {
    async fn get_settings(&self) -> Result<ServerSettings, ServerSettingsError> {
        unimplemented!("text generation never reads settings")
    }
    async fn update_settings(&self, _: ServerSettingsPatch) -> Result<ServerSettings, ServerSettingsError> {
        unimplemented!()
    }
    fn subscribe_changes(&self) -> EventStream<ServerSettings> {
        Box::pin(futures::stream::empty())
    }
}

struct StubSnapshot(ServerProvider);

#[async_trait]
impl ServerProviderSource for StubSnapshot {
    async fn get_snapshot(&self) -> ServerProvider {
        self.0.clone()
    }
    async fn refresh(&self) -> ServerProvider {
        self.0.clone()
    }
    fn subscribe_changes(&self) -> EventStream<ServerProvider> {
        Box::pin(futures::stream::empty())
    }
    async fn resolve_maintenance(&self, _: bool) -> ProviderMaintenanceCapabilities {
        unimplemented!()
    }
    async fn apply_usage_limits(&self, _: ProviderUsageLimitsUpdate, _: String) {}
}

struct StubAdapter(ProviderDriverKind);

#[async_trait]
impl ProviderAdapter for StubAdapter {
    fn provider(&self) -> ProviderDriverKind {
        self.0.clone()
    }
    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            session_model_switch: SessionModelSwitch::Unsupported,
            promptless_turn_continuation: false,
            supports_conversation_rollback: false,
        }
    }
    async fn start_session(&self, _: ProviderSessionStartInput) -> AdapterResult<ProviderSession> {
        unimplemented!()
    }
    async fn send_turn(&self, _: ProviderSendTurnInput) -> AdapterResult<ProviderTurnStartResult> {
        unimplemented!()
    }
    async fn interrupt_turn(&self, _: &ThreadId, _: Option<&TurnId>) -> AdapterResult<()> {
        unimplemented!()
    }
    async fn respond_to_request(&self, _: &ThreadId, _: &ApprovalRequestId, _: ProviderApprovalDecision) -> AdapterResult<()> {
        unimplemented!()
    }
    async fn respond_to_user_input(&self, _: &ThreadId, _: &ApprovalRequestId, _: ProviderUserInputAnswers) -> AdapterResult<()> {
        unimplemented!()
    }
    async fn stop_session(&self, _: &ThreadId) -> AdapterResult<()> {
        unimplemented!()
    }
    async fn list_sessions(&self) -> Vec<ProviderSession> {
        Vec::new()
    }
    async fn has_session(&self, _: &ThreadId) -> bool {
        false
    }
    async fn read_thread(&self, _: &ThreadId) -> AdapterResult<ThreadSnapshot> {
        unimplemented!()
    }
    async fn rollback_thread(&self, _: &ThreadId, _: u32) -> AdapterResult<ThreadSnapshot> {
        unimplemented!()
    }
    async fn stop_all(&self) -> AdapterResult<()> {
        Ok(())
    }
    fn subscribe_events(&self) -> BoxStream<'static, ProviderRuntimeEvent> {
        Box::pin(futures::stream::empty())
    }
}

/// A driver like the provider crates' will be: it builds no text generation itself.
struct StubDriver(&'static str);

#[async_trait]
impl Driver for StubDriver {
    fn driver_kind(&self) -> ProviderDriverKind {
        ProviderDriverKind::from(self.0)
    }
    fn metadata(&self) -> DriverMetadata {
        DriverMetadata {
            display_name: self.0.into(),
            supports_multiple_instances: true,
        }
    }
    fn decode_config(&self, raw: &Value) -> Result<Value, String> {
        Ok(raw.clone())
    }
    fn default_config(&self) -> Value {
        json!({})
    }
    async fn create(&self, input: DriverCreateInput) -> Result<ProviderInstance, ProviderDriverError> {
        let kind = self.driver_kind();
        let snapshot: ServerProvider = serde_json::from_value(json!({
            "instanceId": input.instance_id, "driver": self.0, "enabled": true, "installed": true, "version": null, "status": "ready",
            "auth": {"status": "authenticated"}, "checkedAt": "2026-01-01T00:00:00.000Z",
            "models": [{"slug": "openai.gpt-5.6-luna", "name": "Luna", "isCustom": false, "capabilities": null}]
        }))
        .unwrap();
        Ok(ProviderInstance {
            continuation_identity: ContinuationIdentity::default_for(&kind, &input.instance_id),
            instance_id: input.instance_id,
            driver_kind: kind.clone(),
            display_name: None,
            accent_color: None,
            enabled: input.enabled,
            snapshot: Arc::new(StubSnapshot(snapshot)),
            snapshot_for_cwd: None,
            refresh_models: None,
            invalidate_caches: None,
            consume_reset_credit: None,
            adapter: Arc::new(StubAdapter(kind)),
            text_generation: None,
            auth: None,
        })
    }
}

fn instance(driver: &str, config: Value, fake: &FakeCli) -> ProviderInstanceConfig {
    serde_json::from_value(json!({
        "driver": driver,
        "config": config,
        "environment": [{"name": "FAKE_CLI_DIR", "value": fake.data.to_string_lossy()}]
    }))
    .unwrap()
}

#[tokio::test]
async fn instances_get_the_text_generation_of_their_driver_kind() {
    let claude = FakeCli::new("claude");
    let codex = FakeCli::new("codex");
    let state = tempfile::tempdir().unwrap();
    let env = DriverEnv {
        event_loggers: ProviderEventLoggers::none(),
        model_manifest: ModelManifest::bundled_only(),
        settings: Arc::new(NoSettings),
        background_policy: None,
        server_cwd: state.path().to_path_buf(),
        state_dir: state.path().to_path_buf(),
        attachments_dir: state.path().join("attachments"),
        base_env: HashMap::from([("PATH".to_owned(), "/usr/bin:/bin".to_owned())]),
        mcp_sessions: None,
    };
    let drivers = with_text_generation(
        vec![
            Arc::new(StubDriver("codex")),
            Arc::new(StubDriver("claudeAgent")),
            Arc::new(StubDriver("cursor")),
        ],
        &env,
    );
    let registry = ProviderInstanceRegistry::with_config(
        drivers,
        &[
            (
                ProviderInstanceId::from("claude_work"),
                instance("claudeAgent", json!({"binaryPath": claude.binary.to_string_lossy()}), &claude),
            ),
            (
                ProviderInstanceId::from("codex_work"),
                instance("codex", json!({"binaryPath": codex.binary.to_string_lossy()}), &codex),
            ),
            (ProviderInstanceId::from("cursor"), instance("cursor", json!({}), &claude)),
        ],
    )
    .await;
    let service = TextGenerationService::from_registry(registry.clone(), None);

    claude.stdout(&json!({"structured_output": {"title": "Made-up Claude title"}}).to_string());
    let title = service
        .generate_thread_title(title_input("Name it", selection("claude_work", "made-up-claude-model", None)))
        .await
        .unwrap();
    assert_eq!(title.title, "Made-up Claude title");
    let record = claude.only_record();
    assert_eq!(record.value_after("--model"), Some("made-up-claude-model"));
    assert_eq!(record.env.get("FAKE_CLI_DIR").map(String::as_str), claude.data.to_str());

    codex.output(&json!({"branch": "made-up-branch"}).to_string());
    let branch = service
        .generate_branch_name(branch_input("Do it", selection("codex_work", "gpt-5.6-luna", None)))
        .await
        .unwrap();
    assert_eq!(branch, "made-up-branch");
    // The instance snapshot's qualified model wins (`getModels`).
    assert_eq!(codex.only_record().value_after("--model"), Some("openai.gpt-5.6-luna"));

    let error = service
        .generate_commit_message(commit_input(selection("cursor", "auto", None)))
        .await
        .unwrap_err();
    assert_eq!(
        detail(&error),
        "Cursor text generation is not supported by this server yet. Choose a Claude or Codex model for text generation."
    );
    registry.close().await;
}
