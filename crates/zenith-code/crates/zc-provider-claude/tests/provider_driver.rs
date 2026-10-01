//! `ClaudeProviderDriver`: config decoding and instance creation against fake `claude` binaries
//! (never the real CLI).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use zc_contracts::{ServerProvider, ServerProviderState, ServerProviderVersionAdvisoryStatus, ServerSettings, ServerSettingsPatch};
use zc_ports::contracts::ServerSettingsError;
use zc_ports::{EventStream, SettingsService};
use zc_provider_claude::provider_driver::LatestVersionLookup;
use zc_provider_claude::ClaudeProviderDriver;
use zc_providers::driver::{DriverCreateInput, ProviderInstance};
use zc_providers::{Driver, DriverEnv, InstanceScope, ModelManifest, ProviderEventLoggers};

struct FixedSettings(ServerSettings);

#[async_trait]
impl SettingsService for FixedSettings {
    async fn get_settings(&self) -> Result<ServerSettings, ServerSettingsError> {
        Ok(self.0.clone())
    }

    async fn update_settings(&self, _patch: ServerSettingsPatch) -> Result<ServerSettings, ServerSettingsError> {
        Ok(self.0.clone())
    }

    fn subscribe_changes(&self) -> EventStream<ServerSettings> {
        futures::stream::pending().boxed()
    }
}

fn driver_env(root: &Path, settings: Value) -> DriverEnv {
    DriverEnv {
        event_loggers: ProviderEventLoggers::none(),
        model_manifest: ModelManifest::bundled_only(),
        settings: Arc::new(FixedSettings(serde_json::from_value(settings).unwrap())),
        background_policy: None,
        server_cwd: root.to_path_buf(),
        state_dir: root.join("userdata"),
        attachments_dir: root.join("attachments"),
        base_env: HashMap::from([
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ("HOME".to_owned(), root.to_string_lossy().into_owned()),
        ]),
        mcp_sessions: None,
    }
}

/// A `claude` that answers `--version` (and records every call in `calls.log`).
fn fake_claude(root: &Path, version_output: &str) -> PathBuf {
    let path = root.join("fake-claude");
    let log = root.join("calls.log");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\nif [ \"$1\" = \"--version\" ]; then echo '{version_output}'; exit 0; fi\nexit 1\n",
            log.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn create_input(instance_id: &str, enabled: bool, config: Value) -> DriverCreateInput {
    DriverCreateInput {
        instance_id: instance_id.into(),
        display_name: Some("Claude Work".into()),
        accent_color: Some("#336699".into()),
        environment: Vec::new(),
        enabled,
        config,
        scope: InstanceScope::new(),
    }
}

async fn wait_for(instance: &ProviderInstance, mut done: impl FnMut(&ServerProvider) -> bool) -> ServerProvider {
    for _ in 0..500 {
        let snapshot = instance.snapshot.get_snapshot().await;
        if done(&snapshot) {
            return snapshot;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("snapshot never reached the expected state: {:?}", instance.snapshot.get_snapshot().await);
}

fn no_network() -> LatestVersionLookup {
    Arc::new(|_| Box::pin(async { panic!("the npm registry must not be queried") }))
}

#[test]
fn decodes_configs_with_defaults() {
    let root = tempfile::tempdir().unwrap();
    let driver = ClaudeProviderDriver::new(driver_env(root.path(), json!({})));
    assert_eq!(driver.driver_kind().as_str(), "claudeAgent");
    assert!(driver.metadata().supports_multiple_instances);
    let defaults = driver.default_config();
    assert_eq!(driver.decode_config(&defaults).unwrap(), defaults);
    assert_eq!(driver.decode_config(&json!({})).unwrap(), defaults);
    assert_eq!(defaults["binaryPath"], json!("claude"));
    let custom = driver
        .decode_config(&json!({"binaryPath": "/opt/fake/claude", "homePath": "/tmp/claude-alt"}))
        .unwrap();
    assert_eq!(custom["binaryPath"], json!("/opt/fake/claude"));
    assert_eq!(custom["homePath"], json!("/tmp/claude-alt"));
    assert_eq!(custom["enabled"], defaults["enabled"]);
    assert!(driver.decode_config(&json!({"binaryPath": 5})).is_err());
    assert!(driver.decode_config(&json!(["claude"])).is_err());
}

#[tokio::test]
async fn a_missing_binary_yields_a_not_installed_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("claude-home");
    let driver = ClaudeProviderDriver::new(driver_env(root.path(), json!({}))).with_latest_version_lookup(no_network());
    let config = driver
        .decode_config(&json!({"binaryPath": root.path().join("no-such-claude").to_string_lossy(), "homePath": home.to_string_lossy()}))
        .unwrap();
    let instance = driver.create(create_input("claude_work", true, config)).await.unwrap();

    assert_eq!(instance.driver_kind.as_str(), "claudeAgent");
    assert_eq!(instance.continuation_identity.continuation_key, format!("claude:home:{}", home.display()));
    let initial = instance.snapshot.get_snapshot().await;
    assert_eq!(initial.instance_id.as_str(), "claude_work");
    assert_eq!(initial.driver.as_str(), "claudeAgent");
    assert_eq!(initial.display_name.as_deref(), Some("Claude Work"));
    assert_eq!(initial.accent_color.as_deref(), Some("#336699"));
    assert_eq!(
        initial.continuation.as_ref().map(|c| c.group_key.clone()),
        Some(format!("claude:home:{}", home.display()))
    );
    assert!(!initial.models.is_empty(), "the pending snapshot lists the catalog models");

    let probed = wait_for(&instance, |snapshot| {
        snapshot.status == ServerProviderState::Error && snapshot.version_advisory.is_some()
    })
    .await;
    assert!(!probed.installed);
    assert_eq!(probed.instance_id.as_str(), "claude_work");
    assert_eq!(probed.driver.as_str(), "claudeAgent");
    assert_eq!(probed.message.as_deref(), Some("Claude Agent CLI (`claude`) was not found on PATH."));
    let advisory = probed.version_advisory.unwrap();
    assert_eq!(advisory.status, ServerProviderVersionAdvisoryStatus::Unknown);
    assert!(!advisory.can_update);

    let maintenance = instance.snapshot.resolve_maintenance(true).await;
    assert_eq!(maintenance.package_name.as_deref(), Some("@anthropic-ai/claude-code"));
    assert!(maintenance.update.is_none());

    // Nothing to redeem without a version and a banked reset.
    let outcome = (instance.consume_reset_credit.as_ref().unwrap())().await.unwrap();
    assert_eq!(outcome, json!("noCredit"));
}

#[tokio::test]
async fn an_installed_binary_gets_its_version_and_advisory() {
    let root = tempfile::tempdir().unwrap();
    let binary = fake_claude(root.path(), "2.1.0 (Claude Code)");
    let lookups = Arc::new(Mutex::new(Vec::<String>::new()));
    let lookup: LatestVersionLookup = {
        let lookups = lookups.clone();
        Arc::new(move |package| {
            lookups.lock().unwrap().push(package);
            Box::pin(async { Some("2.2.0".to_owned()) })
        })
    };
    let driver = ClaudeProviderDriver::new(driver_env(root.path(), json!({"enableProviderUpdateChecks": true}))).with_latest_version_lookup(lookup);
    let config = driver
        .decode_config(&json!({"binaryPath": binary.to_string_lossy(), "homePath": root.path().join("claude-home").to_string_lossy()}))
        .unwrap();
    let instance = driver.create(create_input("claude_fake", true, config)).await.unwrap();

    let probed = wait_for(&instance, |snapshot| {
        snapshot.version_advisory.as_ref().is_some_and(|a| a.latest_version.is_some())
    })
    .await;
    assert!(probed.installed);
    assert_eq!(probed.version.as_deref(), Some("2.1.0"));
    assert_eq!(probed.instance_id.as_str(), "claude_fake");
    let advisory = probed.version_advisory.unwrap();
    assert_eq!(advisory.status, ServerProviderVersionAdvisoryStatus::BehindLatest);
    assert_eq!(advisory.current_version.as_deref(), Some("2.1.0"));
    assert_eq!(advisory.latest_version.as_deref(), Some("2.2.0"));
    assert_eq!(lookups.lock().unwrap().first().map(String::as_str), Some("@anthropic-ai/claude-code"));
}

#[tokio::test]
async fn a_disabled_instance_never_runs_its_binary() {
    let root = tempfile::tempdir().unwrap();
    let binary = fake_claude(root.path(), "2.1.0 (Claude Code)");
    let driver = ClaudeProviderDriver::new(driver_env(root.path(), json!({}))).with_latest_version_lookup(no_network());
    let config = driver.decode_config(&json!({"binaryPath": binary.to_string_lossy()})).unwrap();
    let instance = driver.create(create_input("claude_off", false, config)).await.unwrap();
    assert!(!instance.enabled);
    let snapshot = wait_for(&instance, |snapshot| snapshot.version_advisory.is_some()).await;
    assert_eq!(snapshot.status, ServerProviderState::Disabled);
    assert!(!snapshot.enabled);
    assert_eq!(snapshot.message.as_deref(), Some("Claude is disabled in zenith code settings."));
    let scoped = (instance.snapshot_for_cwd.as_ref().unwrap())(root.path().to_string_lossy().into_owned())
        .await
        .unwrap();
    assert!(scoped.skills.is_empty());
    assert!(!root.path().join("calls.log").exists(), "a disabled instance must not probe");
}

#[tokio::test]
async fn workspace_snapshots_list_project_skills() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let skill_dir = workspace.join(".claude/skills/tidy-notes");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), "---\ndescription: Tidy the notes.\n---\nBody\n").unwrap();
    let driver = ClaudeProviderDriver::new(driver_env(root.path(), json!({}))).with_latest_version_lookup(no_network());
    let config = driver
        .decode_config(
            &json!({"binaryPath": root.path().join("no-such-claude").to_string_lossy(), "homePath": root.path().join("claude-home").to_string_lossy()}),
        )
        .unwrap();
    let instance = driver.create(create_input("claude_skills", true, config)).await.unwrap();
    let scoped = (instance.snapshot_for_cwd.as_ref().unwrap())(workspace.to_string_lossy().into_owned())
        .await
        .unwrap();
    assert_eq!(scoped.instance_id.as_str(), "claude_skills");
    let skills = serde_json::to_value(&scoped.skills).unwrap();
    assert_eq!(skills[0]["name"], json!("tidy-notes"));
    assert_eq!(skills[0]["scope"], json!("project"));
    assert_eq!(skills[0]["description"], json!("Tidy the notes."));
}

#[tokio::test]
async fn closing_the_scope_shuts_the_adapter_down() {
    let root = tempfile::tempdir().unwrap();
    let driver = ClaudeProviderDriver::new(driver_env(root.path(), json!({}))).with_latest_version_lookup(no_network());
    let config = driver
        .decode_config(&json!({"binaryPath": root.path().join("no-such-claude").to_string_lossy()}))
        .unwrap();
    let input = create_input("claude_closing", true, config);
    let scope = input.scope.clone();
    let instance = driver.create(input).await.unwrap();
    let mut events = instance.adapter.subscribe_events();
    let mut changes = instance.snapshot.subscribe_changes();
    scope.close().await;
    let ended = tokio::time::timeout(Duration::from_secs(2), events.next())
        .await
        .expect("the adapter event stream ends");
    assert!(ended.is_none());
    let changes_ended = tokio::time::timeout(Duration::from_secs(2), async { while changes.next().await.is_some() {} }).await;
    assert!(changes_ended.is_ok(), "the snapshot change stream ends with the scope");
}
