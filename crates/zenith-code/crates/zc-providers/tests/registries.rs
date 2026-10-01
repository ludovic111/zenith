//! Ports of `ProviderInstanceRegistryLive.test.ts`, `ProviderAdapterRegistry.test.ts`,
//! `ProviderRegistry.test.ts` (driver-neutral parts) and `makeManagedServerProvider.test.ts`,
//! over fake drivers whose probes are scripted (no provider CLI runs).

mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use common::drivers::{FakeDriver, ProbeScript};
use common::{eventually, MemorySettings};
use futures::StreamExt;
use serde_json::{json, Value};
use zc_contracts::{ProviderInstanceConfig, ProviderInstanceId, ServerProvider, ServerProviderAvailability, ServerProviderState, ServerProviderUpdateState};
use zc_ports::adapter::AdapterError;
use zc_providers::adapter_registry::{AdapterRegistry, InstanceAdapterRegistry};
use zc_providers::driver::Driver;
use zc_providers::instance_registry::hydrate;
use zc_providers::manifest::ModelManifest;
use zc_providers::status_cache::{read_provider_status_cache, resolve_provider_status_cache_path, write_provider_status_cache};
use zc_providers::{ProviderInstanceRegistry, ProviderRegistry};

fn entry(value: Value) -> ProviderInstanceConfig {
    serde_json::from_value(value).unwrap()
}

fn config_map(entries: Vec<(&str, Value)>) -> Vec<(ProviderInstanceId, ProviderInstanceConfig)> {
    entries.into_iter().map(|(id, value)| (ProviderInstanceId::from(id), entry(value))).collect()
}

fn drivers(list: Vec<Arc<FakeDriver>>) -> Vec<Arc<dyn Driver>> {
    list.iter().map(|driver| driver.clone() as Arc<dyn Driver>).collect()
}

// ---------------------------------------------------------------------------------------------
// ProviderInstanceRegistry
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn boots_independent_instances_of_one_driver() {
    let codex = FakeDriver::new("codex");
    let registry = ProviderInstanceRegistry::with_config(
        drivers(vec![codex.clone()]),
        &config_map(vec![
            (
                "codex_personal",
                json!({"driver": "codex", "displayName": "Personal", "config": {"binaryPath": "/opt/personal/codex"}}),
            ),
            (
                "codex_work",
                json!({"driver": "codex", "displayName": "Work", "accentColor": "#123456", "config": {"binaryPath": "/opt/work/codex"}}),
            ),
        ]),
    )
    .await;
    let instances = registry.list_instances();
    let ids: Vec<&str> = instances.iter().map(|instance| instance.instance_id.as_str()).collect();
    assert_eq!(ids, vec!["codex_personal", "codex_work"]);
    assert!(!Arc::ptr_eq(&instances[0].adapter, &instances[1].adapter));
    assert_eq!(instances[1].accent_color.as_deref(), Some("#123456"));
    assert_eq!(instances[0].continuation_identity.continuation_key, "codex:instance:codex_personal");
    let created = codex.created.lock().unwrap();
    assert_eq!(created[0].config["binaryPath"], json!("/opt/personal/codex"));
    assert_eq!(created[1].config["binaryPath"], json!("/opt/work/codex"));
    assert!(registry.list_unavailable().is_empty());
}

#[tokio::test]
async fn an_explicit_config_disable_wins_over_the_envelope() {
    let codex = FakeDriver::new("codex");
    let registry = ProviderInstanceRegistry::with_config(
        drivers(vec![codex.clone()]),
        &config_map(vec![("codex", json!({"driver": "codex", "enabled": true, "config": {"enabled": false}}))]),
    )
    .await;
    assert!(!registry.get_instance(&"codex".into()).unwrap().enabled);
}

#[tokio::test]
async fn unknown_drivers_bad_configs_and_failed_creates_become_unavailable_shadows() {
    let codex = FakeDriver::new("codex");
    let broken = FakeDriver::new("cursor");
    *broken.fail_create.lock().unwrap() = true;
    let registry = ProviderInstanceRegistry::with_config(
        drivers(vec![codex.clone(), broken.clone()]),
        &config_map(vec![
            ("fork_one", json!({"driver": "forkDriver", "displayName": "Fork"})),
            ("codex_bad", json!({"driver": "codex", "config": {"binaryPath": 42}})),
            ("cursor", json!({"driver": "cursor"})),
            ("codex", json!({"driver": "codex"})),
        ]),
    )
    .await;
    let ids: Vec<String> = registry.list_instances().iter().map(|instance| instance.instance_id.to_string()).collect();
    assert_eq!(ids, vec!["codex"]);
    let unavailable = registry.list_unavailable();
    let reasons: Vec<(String, String)> = unavailable
        .iter()
        .map(|snapshot| (snapshot.instance_id.to_string(), snapshot.unavailable_reason.clone().unwrap()))
        .collect();
    assert_eq!(
        reasons,
        vec![
            ("fork_one".to_owned(), "Driver 'forkDriver' is not registered in this build.".to_owned()),
            (
                "codex_bad".to_owned(),
                "Invalid config for instance 'codex_bad': Expected string at [\"binaryPath\"]".to_owned()
            ),
            ("cursor".to_owned(), "Driver 'cursor' failed to create instance: binary exploded".to_owned()),
        ]
    );
    assert!(unavailable
        .iter()
        .all(|snapshot| snapshot.availability == Some(ServerProviderAvailability::Unavailable) && !snapshot.enabled));
    assert_eq!(unavailable[0].display_name.as_deref(), Some("Fork"));
}

#[tokio::test]
async fn reconcile_keeps_unchanged_instances_and_replaces_changed_ones_one_at_a_time() {
    let codex = FakeDriver::new("codex");
    let registry = ProviderInstanceRegistry::new(drivers(vec![codex.clone()]));
    let mut changes = registry.subscribe_changes();
    let initial = config_map(vec![
        ("codex", json!({"driver": "codex"})),
        ("codex_work", json!({"driver": "codex", "config": {"binaryPath": "/a"}})),
    ]);
    registry.reconcile(&initial).await;
    assert_eq!(changes.drain_ready().len(), 1);
    let first = registry.get_instance(&"codex".into()).unwrap();
    let (_, _, work_scope) = codex.last("codex_work");

    // Same map: no churn, no tick.
    registry.reconcile(&initial).await;
    assert!(changes.drain_ready().is_empty());
    assert!(Arc::ptr_eq(&first, &registry.get_instance(&"codex".into()).unwrap()));

    // Changing one entry closes its scope before the replacement is created.
    let changed = config_map(vec![
        ("codex", json!({"driver": "codex"})),
        ("codex_work", json!({"driver": "codex", "config": {"binaryPath": "/b"}})),
    ]);
    registry.reconcile(&changed).await;
    assert!(work_scope.is_closed());
    let (old_adapter, _, _) = {
        let created = codex.created.lock().unwrap();
        let old = created.iter().find(|created| created.instance_id == "codex_work").unwrap();
        (old.adapter.clone(), old.probe.clone(), old.scope.clone())
    };
    assert!(old_adapter.calls().contains(&common::Call::StopAll));
    assert_eq!(codex.created_ids(), vec!["codex", "codex_work", "codex_work"]);
    assert!(Arc::ptr_eq(&first, &registry.get_instance(&"codex".into()).unwrap()));
    assert_eq!(changes.drain_ready().len(), 1);

    // Removing one closes it.
    let (_, _, replacement_scope) = codex.last("codex_work");
    registry.reconcile(&config_map(vec![("codex", json!({"driver": "codex"}))])).await;
    assert!(replacement_scope.is_closed());
    assert_eq!(registry.list_instances().len(), 1);
    assert_eq!(changes.drain_ready().len(), 1);

    registry.close().await;
    assert!(codex.last("codex").2.is_closed());
}

#[tokio::test]
async fn single_instance_drivers_refuse_a_second_instance() {
    let mut desktop = FakeDriver::new("desktopOnly");
    Arc::get_mut(&mut desktop).unwrap().multiple = false;
    let registry = ProviderInstanceRegistry::with_config(
        drivers(vec![desktop.clone()]),
        &config_map(vec![
            ("desktopOnly", json!({"driver": "desktopOnly"})),
            ("desktop_two", json!({"driver": "desktopOnly"})),
        ]),
    )
    .await;
    assert_eq!(registry.list_instances().len(), 1);
    assert_eq!(
        registry.list_unavailable()[0].unavailable_reason.as_deref(),
        Some("Driver 'desktopOnly' supports only one instance.")
    );
}

#[tokio::test]
async fn hydration_follows_settings_changes() {
    let codex = FakeDriver::new("codex");
    let claude = FakeDriver::new("claudeAgent");
    let settings = MemorySettings::new(json!({"providers": {"codex": {"binaryPath": "/legacy/codex"}}}));
    let (registry, watcher) = hydrate(drivers(vec![codex.clone(), claude.clone()]), settings.clone()).await;
    let ids = |registry: &ProviderInstanceRegistry| {
        registry
            .list_instances()
            .iter()
            .map(|instance| instance.instance_id.to_string())
            .collect::<Vec<_>>()
    };
    // Both built-ins get their default instance; codex from its legacy blob.
    assert_eq!(ids(&registry), vec!["codex", "claudeAgent"]);
    assert_eq!(codex.created.lock().unwrap()[0].config["binaryPath"], json!("/legacy/codex"));

    settings.set(json!({
        "providers": {"codex": {"binaryPath": "/legacy/codex"}},
        "providerInstances": {"codex_work": {"driver": "codex", "config": {"binaryPath": "/work"}}}
    }));
    eventually(|| registry.list_instances().len() == 3).await;
    assert_eq!(ids(&registry), vec!["codex_work", "codex", "claudeAgent"]);
    watcher.abort();
    registry.close().await;
}

struct ChangingCredentials;

#[async_trait]
impl zc_providers::driver::ProviderAuthAdmission for ChangingCredentials {
    fn credential_binding(&self) -> Option<(String, String)> {
        Some(("t3".into(), "shared-login".into()))
    }
    async fn is_changing_credentials(&self) -> bool {
        true
    }
}

struct AuthDriver(Arc<FakeDriver>);

#[async_trait]
impl Driver for AuthDriver {
    fn driver_kind(&self) -> zc_contracts::ProviderDriverKind {
        self.0.driver_kind()
    }
    fn metadata(&self) -> zc_providers::DriverMetadata {
        self.0.metadata()
    }
    fn decode_config(&self, raw: &Value) -> Result<Value, String> {
        self.0.decode_config(raw)
    }
    fn default_config(&self) -> Value {
        self.0.default_config()
    }
    async fn create(&self, input: zc_providers::DriverCreateInput) -> Result<zc_providers::ProviderInstance, zc_providers::ProviderDriverError> {
        let mut instance = self.0.create(input).await?;
        instance.auth = Some(Arc::new(ChangingCredentials));
        Ok(instance)
    }
}

#[tokio::test]
async fn the_adapter_registry_resolves_routing_and_guards_shared_credentials() {
    let codex = FakeDriver::new("codex");
    let registry = ProviderInstanceRegistry::with_config(
        vec![Arc::new(AuthDriver(codex.clone())) as Arc<dyn Driver>],
        &config_map(vec![("codex", json!({"driver": "codex", "displayName": "Codex"}))]),
    )
    .await;
    let adapters = InstanceAdapterRegistry::new(registry.clone());
    let info = adapters.get_instance_info(&"codex".into()).unwrap();
    assert_eq!(info.display_name.as_deref(), Some("Codex"));
    assert_eq!(info.driver_kind.as_str(), "codex");
    assert_eq!(adapters.list_instances(), vec![ProviderInstanceId::from("codex")]);
    let missing = adapters.get_by_instance(&"nope".into()).err().unwrap();
    assert_eq!(missing.tag(), "ProviderUnsupportedError");

    // Stable identity across lookups (the provider service subscribes once).
    let first = adapters.get_by_instance(&"codex".into()).unwrap();
    let second = adapters.get_by_instance(&"codex".into()).unwrap();
    assert!(std::ptr::eq(Arc::as_ptr(&first) as *const (), Arc::as_ptr(&second) as *const ()));
    let error = first
        .start_session(serde_json::from_value(json!({"threadId": "t", "providerInstanceId": "codex", "runtimeMode": "full-access"})).unwrap())
        .await
        .unwrap_err();
    assert!(matches!(error, AdapterError::Validation { ref issue, .. } if issue == "Provider sign-in is changing. Try again after it finishes."));
    assert!(codex.last("codex").0.calls().is_empty());
}

// ---------------------------------------------------------------------------------------------
// ProviderRegistry
// ---------------------------------------------------------------------------------------------

async fn provider_registry(instances: &ProviderInstanceRegistry, cache_dir: &std::path::Path) -> ProviderRegistry {
    ProviderRegistry::start(instances.clone(), ModelManifest::bundled_only(), cache_dir.to_path_buf()).await
}

fn statuses(providers: &[ServerProvider]) -> Vec<(String, ServerProviderState)> {
    providers.iter().map(|provider| (provider.instance_id.to_string(), provider.status)).collect()
}

#[tokio::test]
async fn construction_never_waits_for_probes_and_live_results_arrive_on_the_stream() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let codex = FakeDriver::gated("codex", gate.clone());
    let cache = tempfile::tempdir().unwrap();
    let instances = ProviderInstanceRegistry::with_config(drivers(vec![codex.clone()]), &config_map(vec![("codex", json!({"driver": "codex"}))])).await;
    let registry = provider_registry(&instances, cache.path()).await;
    let mut changes = registry.subscribe_changes();
    assert_eq!(statuses(&registry.get_providers()), vec![("codex".to_owned(), ServerProviderState::Warning)]);
    assert_eq!(codex.last("codex").1.checks.load(Ordering::SeqCst), 0);

    gate.add_permits(1);
    let published = tokio::time::timeout(Duration::from_secs(2), changes.recv()).await.unwrap().unwrap();
    assert_eq!(statuses(&published), vec![("codex".to_owned(), ServerProviderState::Ready)]);
    // The probe result is persisted per instance (without workspace snapshots).
    let cached = read_provider_status_cache(&resolve_provider_status_cache_path(cache.path(), "codex"))
        .await
        .unwrap();
    assert_eq!(cached.status, ServerProviderState::Ready);
    // The bundled policy classified the version.
    assert!(published[0].compatibility_advisory.is_some());
    registry.close();
}

#[tokio::test]
async fn boots_from_the_cache_and_includes_unavailable_instances() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let codex = FakeDriver::gated("codex", gate);
    let cache = tempfile::tempdir().unwrap();
    let mut cached: ServerProvider = serde_json::from_value(json!({
        "instanceId": "codex", "driver": "codex", "displayName": "Codex", "enabled": true, "installed": true, "version": "0.156.1",
        "status": "ready", "auth": {"status": "authenticated"}, "checkedAt": "2026-01-01T00:00:00.000Z",
        "models": [{"slug": "cached-model", "name": "Cached", "isCustom": false, "capabilities": null}], "slashCommands": [], "skills": []
    }))
    .unwrap();
    write_provider_status_cache(&resolve_provider_status_cache_path(cache.path(), "codex"), &cached)
        .await
        .unwrap();
    // A cache naming another driver is ignored.
    cached.driver = "cursor".into();
    write_provider_status_cache(&resolve_provider_status_cache_path(cache.path(), "codex_other"), &cached)
        .await
        .unwrap();
    let instances = ProviderInstanceRegistry::with_config(
        drivers(vec![codex.clone()]),
        &config_map(vec![
            ("codex", json!({"driver": "codex"})),
            ("codex_other", json!({"driver": "codex"})),
            ("fork", json!({"driver": "forkDriver"})),
        ]),
    )
    .await;
    let registry = provider_registry(&instances, cache.path()).await;
    let providers = registry.get_providers();
    // As in TS, the pending boot snapshot is merged over the cache: its status shows, and the
    // cached models are kept until the first probe reports.
    let codex_snapshot = providers.iter().find(|provider| provider.instance_id.as_str() == "codex").unwrap();
    assert_eq!(codex_snapshot.status, ServerProviderState::Warning);
    assert_eq!(
        codex_snapshot.models.iter().map(|model| model.slug.as_str()).collect::<Vec<_>>(),
        vec!["cached-model"]
    );
    let other = providers.iter().find(|provider| provider.instance_id.as_str() == "codex_other").unwrap();
    assert_eq!(other.status, ServerProviderState::Warning);
    assert!(other.models.is_empty());
    let fork = providers.iter().find(|provider| provider.instance_id.as_str() == "fork").unwrap();
    assert_eq!(fork.availability, Some(ServerProviderAvailability::Unavailable));
    // Built-in drivers first.
    assert_eq!(providers.last().unwrap().instance_id.as_str(), "fork");
    registry.close();
}

#[tokio::test]
async fn follows_instance_changes_and_drops_ghosts() {
    let codex = FakeDriver::new("codex");
    let cache = tempfile::tempdir().unwrap();
    let instances = ProviderInstanceRegistry::with_config(drivers(vec![codex.clone()]), &config_map(vec![("codex", json!({"driver": "codex"}))])).await;
    let registry = provider_registry(&instances, cache.path()).await;
    eventually(|| registry.get_providers().iter().any(|provider| provider.status == ServerProviderState::Ready)).await;

    instances
        .reconcile(&config_map(vec![
            ("codex", json!({"driver": "codex"})),
            ("codex_work", json!({"driver": "codex", "displayName": "Work"})),
        ]))
        .await;
    eventually(|| registry.get_providers().len() == 2 && registry.get_providers().iter().all(|provider| provider.status == ServerProviderState::Ready)).await;

    instances
        .reconcile(&config_map(vec![("codex_work", json!({"driver": "codex", "displayName": "Work"}))]))
        .await;
    eventually(|| registry.get_providers().len() == 1).await;
    assert_eq!(registry.get_providers()[0].instance_id.as_str(), "codex_work");
    registry.close();
}

#[tokio::test]
async fn refreshes_probe_now_and_unknown_instances_answer_the_cache() {
    let codex = FakeDriver::new("codex");
    let cache = tempfile::tempdir().unwrap();
    let instances = ProviderInstanceRegistry::with_config(drivers(vec![codex.clone()]), &config_map(vec![("codex", json!({"driver": "codex"}))])).await;
    let registry = provider_registry(&instances, cache.path()).await;
    let probe = codex.last("codex").1;
    eventually(|| probe.checks.load(Ordering::SeqCst) == 1).await;
    *probe.script.lock().unwrap() = ProbeScript {
        version: Some("2.0.0".into()),
        ..Default::default()
    };
    let providers = registry.refresh_instance(&"codex".into()).await;
    assert_eq!(probe.checks.load(Ordering::SeqCst), 2);
    assert_eq!(providers[0].version.as_deref(), Some("2.0.0"));
    let unchanged = registry.refresh_instance(&"nope".into()).await;
    assert_eq!(unchanged, registry.get_providers());
    let all = registry.refresh(None).await;
    assert_eq!(probe.checks.load(Ordering::SeqCst), 3);
    assert_eq!(all.len(), 1);
    // A failed probe keeps the models it did not report.
    *probe.script.lock().unwrap() = ProbeScript {
        fail: true,
        ..Default::default()
    };
    let after_failure = registry.refresh(Some(&"codex".into())).await;
    assert_eq!(after_failure[0].status, ServerProviderState::Error);
    assert_eq!(after_failure[0].models[0].slug, "model-a");
    registry.close();
}

#[tokio::test]
async fn workspace_snapshots_are_deduplicated_and_cleared_on_rebuild() {
    let driver = FakeDriver::with_cwd_snapshots("claudeAgent");
    let cache = tempfile::tempdir().unwrap();
    let instances = ProviderInstanceRegistry::with_config(
        drivers(vec![driver.clone()]),
        &config_map(vec![("claudeAgent", json!({"driver": "claudeAgent"}))]),
    )
    .await;
    let registry = provider_registry(&instances, cache.path()).await;
    let claude_id = ProviderInstanceId::from("claudeAgent");
    let (first, second) = tokio::join!(
        registry.refresh_workspace_snapshot(&claude_id, "/work/a"),
        registry.refresh_workspace_snapshot(&claude_id, "/work/a")
    );
    assert_eq!(driver.cwd_calls.load(Ordering::SeqCst), 1);
    let with_workspace = if first[0].workspace_snapshots.is_some() { first } else { second };
    let snapshots = with_workspace[0].workspace_snapshots.clone().unwrap();
    assert_eq!(snapshots[0].cwd, "/work/a");
    assert_eq!(snapshots[0].skills[0].name, "skill-work-a");
    // Known cwds answer without probing.
    registry.refresh_workspace_snapshot(&"claudeAgent".into(), "/work/a").await;
    assert_eq!(driver.cwd_calls.load(Ordering::SeqCst), 1);
    // The workspace data is never written to the cache.
    eventually(|| registry.get_providers()[0].status == ServerProviderState::Ready).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    let cached = read_provider_status_cache(&resolve_provider_status_cache_path(cache.path(), "claudeAgent"))
        .await
        .unwrap();
    assert!(cached.workspace_snapshots.is_none());
    // Rebuilding the instance clears them.
    instances
        .reconcile(&config_map(vec![(
            "claudeAgent",
            json!({"driver": "claudeAgent", "config": {"binaryPath": "/new"}}),
        )]))
        .await;
    eventually(|| registry.get_providers()[0].workspace_snapshots.is_none()).await;
    registry.close();
}

#[tokio::test]
async fn maintenance_action_state_is_volatile_and_projected() {
    let codex = FakeDriver::new("codex");
    let cache = tempfile::tempdir().unwrap();
    let instances = ProviderInstanceRegistry::with_config(drivers(vec![codex.clone()]), &config_map(vec![("codex", json!({"driver": "codex"}))])).await;
    let registry = provider_registry(&instances, cache.path()).await;
    let running: ServerProviderUpdateState =
        serde_json::from_value(json!({"status": "running", "startedAt": "2026-01-01T00:00:00.000Z", "finishedAt": null, "message": null, "output": null}))
            .unwrap();
    let providers = registry.set_provider_maintenance_action_state(&"codex".into(), Some(running.clone())).await;
    assert_eq!(providers[0].update_state, Some(running));
    let idle: ServerProviderUpdateState =
        serde_json::from_value(json!({"status": "idle", "startedAt": null, "finishedAt": null, "message": null, "output": null})).unwrap();
    let providers = registry.set_provider_maintenance_action_state(&"codex".into(), Some(idle)).await;
    assert_eq!(providers[0].update_state, None);
    let capabilities = registry
        .get_provider_maintenance_capabilities_for_instance(&"codex".into(), &"codex".into(), true)
        .await;
    assert!(capabilities.update.is_none());
    registry.close();
}

#[tokio::test]
async fn the_status_port_reads_the_same_list() {
    let codex = FakeDriver::new("codex");
    let cache = tempfile::tempdir().unwrap();
    let instances = ProviderInstanceRegistry::with_config(drivers(vec![codex.clone()]), &config_map(vec![("codex", json!({"driver": "codex"}))])).await;
    let registry = provider_registry(&instances, cache.path()).await;
    let port: Arc<dyn zc_ports::ProviderStatusReads> = Arc::new(registry.clone());
    let mut stream = zc_providers::ports::provider_status_changes(&registry);
    eventually(|| registry.get_providers()[0].status == ServerProviderState::Ready).await;
    let listed = port.get_providers().await;
    assert_eq!(listed[0].0["instanceId"], json!("codex"));
    let first = tokio::time::timeout(Duration::from_millis(200), stream.next()).await.ok().flatten();
    assert!(first.is_none() || first.unwrap()[0].0["driver"] == json!("codex"));
    registry.close();
}

#[tokio::test]
async fn rate_limit_events_reach_the_owning_instance_snapshot() {
    let codex = FakeDriver::new("codex");
    let cache = tempfile::tempdir().unwrap();
    let instances = ProviderInstanceRegistry::with_config(drivers(vec![codex.clone()]), &config_map(vec![("codex", json!({"driver": "codex"}))])).await;
    let registry = provider_registry(&instances, cache.path()).await;
    let db = zc_db::Db::open_in_memory().unwrap();
    let service = zc_providers::ProviderServiceImpl::start(
        Arc::new(InstanceAdapterRegistry::new(instances.clone())),
        zc_providers::ProviderSessionDirectory::new(db),
        zc_providers::ProviderServiceOptions::new(cache.path().join("attachments")),
    )
    .await;
    let stop = tokio_util::sync::CancellationToken::new();
    let task = zc_providers::registry::start_usage_limits_ingestion(&service, instances.clone(), stop.clone());
    eventually(|| registry.get_providers()[0].status == ServerProviderState::Ready).await;
    codex.last("codex").0.emit_json(json!({
        "type": "account.rate-limits.updated", "provider": "codex", "threadId": "t",
        "payload": {"limits": {"windows": [{"id": "weekly", "kind": "weekly", "label": "Weekly", "usedPercent": 64}]}}
    }));
    eventually(|| {
        registry.get_providers()[0]
            .usage_limits
            .as_ref()
            .is_some_and(|limits| limits.windows.0.len() == 1)
    })
    .await;
    assert_eq!(registry.get_providers()[0].usage_limits.as_ref().unwrap().windows.0[0].used_percent.0, 64.0);
    stop.cancel();
    task.await.unwrap();
    registry.close();
}

// ---------------------------------------------------------------------------------------------
// ManagedServerProvider
// ---------------------------------------------------------------------------------------------

mod managed {
    use super::*;
    use common::drivers::FakeProbe;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;
    use zc_ports::contracts::{BackgroundPolicySnapshot, BackgroundScope, ClientActivityReportInput, HostPowerSnapshot};
    use zc_ports::BackgroundPolicy;
    use zc_providers::driver::ServerProviderSource;
    use zc_providers::managed::{ManagedServerProvider, ManagedServerProviderOptions};
    use zc_providers::snapshot::InstanceIdentity;
    use zc_providers::InstanceScope;

    fn probe(enrichment: Option<&str>, probe_on_settings_change: bool) -> Arc<FakeProbe> {
        Arc::new(FakeProbe {
            identity: InstanceIdentity {
                instance_id: "codex".into(),
                driver_kind: "codex".into(),
                display_name: None,
                accent_color: None,
                continuation_group_key: "codex:instance:codex".into(),
            },
            enabled: true,
            settings: Mutex::new(json!({"binaryPath": "codex"})),
            script: Mutex::new(ProbeScript::default()),
            checks: AtomicUsize::new(0),
            gate: None,
            enrichment: Mutex::new(enrichment.map(str::to_owned)),
            probe_on_settings_change,
        })
    }

    struct Demand(bool);

    #[async_trait]
    impl BackgroundPolicy for Demand {
        async fn report_client_activity(&self, _: &zc_ports::contracts::AuthSessionId, _: &zc_ports::contracts::RpcClientId, _: ClientActivityReportInput) {}
        async fn remove_rpc_client(&self, _: &zc_ports::contracts::AuthSessionId, _: &zc_ports::contracts::RpcClientId) {}
        async fn report_host_power_state(&self, _: HostPowerSnapshot) {}
        async fn snapshot(&self) -> BackgroundPolicySnapshot {
            BackgroundPolicySnapshot(json!({}))
        }
        async fn subscribe(&self) -> zc_ports::BackgroundPolicySubscription {
            zc_ports::BackgroundPolicySubscription {
                latest: BackgroundPolicySnapshot(json!({})),
                changes: futures::stream::empty().boxed(),
            }
        }
        async fn has_demand(&self, _: &BackgroundScope) -> bool {
            self.0
        }
        async fn should_run_scope_work(&self, _: &BackgroundScope) -> bool {
            self.0
        }
        async fn should_run_opportunistic_work(&self) -> bool {
            self.0
        }
    }

    async fn start(
        probe: Arc<FakeProbe>,
        settings: futures::stream::BoxStream<'static, Value>,
        interval: Option<i64>,
        demand: Option<bool>,
    ) -> (ManagedServerProvider<FakeProbe>, InstanceScope) {
        let scope = InstanceScope::new();
        let mut options = ManagedServerProviderOptions::new(scope.clone());
        options.refresh_interval_ms = interval;
        options.background_policy = demand.map(|demand| Arc::new(Demand(demand)) as Arc<dyn BackgroundPolicy>);
        (ManagedServerProvider::start(probe, settings, options).await.unwrap(), scope)
    }

    #[tokio::test]
    async fn publishes_the_pending_snapshot_then_the_probe() {
        let probe = probe(None, true);
        let (provider, scope) = start(probe.clone(), futures::stream::empty().boxed(), Some(0), None).await;
        let mut changes = provider.subscribe_changes();
        eventually(|| probe.checks.load(Ordering::SeqCst) == 1).await;
        let current = provider.get_snapshot().await;
        assert_eq!(current.status, ServerProviderState::Ready);
        let _ = tokio::time::timeout(Duration::from_millis(100), changes.next()).await;
        scope.close().await;
        // Closing the scope ends the change stream.
        assert!(tokio::time::timeout(Duration::from_secs(1), async { while changes.next().await.is_some() {} })
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn periodic_refresh_needs_provider_status_demand() {
        for (demand, expect_more) in [(false, false), (true, true)] {
            let probe = probe(None, true);
            let (_provider, scope) = start(probe.clone(), futures::stream::empty().boxed(), Some(30), Some(demand)).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            let checks = probe.checks.load(Ordering::SeqCst);
            assert_eq!(checks > 1, expect_more, "demand {demand}: {checks} checks");
            scope.close().await;
        }
        // An explicit zero interval disables periodic refreshes; manual refresh still works.
        let probe = probe(None, true);
        let (provider, scope) = start(probe.clone(), futures::stream::empty().boxed(), Some(0), Some(true)).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(probe.checks.load(Ordering::SeqCst), 1);
        provider.refresh().await;
        assert_eq!(probe.checks.load(Ordering::SeqCst), 2);
        scope.close().await;
    }

    #[tokio::test]
    async fn settings_changes_reprobe_or_only_reenrich() {
        let (sender, receiver) = futures::channel::mpsc::unbounded::<Value>();
        let probe_a = probe(None, true);
        let (_provider, scope) = start(probe_a.clone(), receiver.boxed(), Some(0), None).await;
        eventually(|| probe_a.checks.load(Ordering::SeqCst) == 1).await;
        sender.unbounded_send(json!({"binaryPath": "codex"})).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(probe_a.checks.load(Ordering::SeqCst), 1, "unchanged settings do not probe");
        sender.unbounded_send(json!({"binaryPath": "/new/codex"})).unwrap();
        eventually(|| probe_a.checks.load(Ordering::SeqCst) == 2).await;
        scope.close().await;

        let (sender, receiver) = futures::channel::mpsc::unbounded::<Value>();
        let probe_b = probe(Some("enriched"), false);
        let (provider, scope) = start(probe_b.clone(), receiver.boxed(), Some(0), None).await;
        eventually(|| probe_b.checks.load(Ordering::SeqCst) == 1).await;
        eventually(|| provider.current().message.as_deref() == Some("enriched")).await;
        *probe_b.enrichment.lock().unwrap() = Some("re-enriched".into());
        sender.unbounded_send(json!({"enableProviderUpdateChecks": false})).unwrap();
        eventually(|| provider.current().message.as_deref() == Some("re-enriched")).await;
        assert_eq!(probe_b.checks.load(Ordering::SeqCst), 1);
        scope.close().await;
    }

    #[tokio::test]
    async fn runtime_usage_updates_patch_the_published_snapshot_and_survive_failed_probes() {
        let probe = probe(None, true);
        let (provider, scope) = start(probe.clone(), futures::stream::empty().boxed(), Some(0), None).await;
        eventually(|| probe.checks.load(Ordering::SeqCst) == 1).await;
        let mut changes = provider.subscribe_changes();
        let update: zc_contracts::ProviderUsageLimitsUpdate =
            serde_json::from_value(json!({"windows": [{"id": "weekly", "kind": "weekly", "label": "Weekly", "usedPercent": 42}]})).unwrap();
        provider.apply_usage_limits(update.clone(), "2026-01-01T00:00:00.000Z".into()).await;
        let published = tokio::time::timeout(Duration::from_secs(1), changes.next()).await.unwrap().unwrap();
        assert_eq!(published.usage_limits.as_ref().unwrap().windows.0[0].used_percent.0, 42.0);
        // The same numbers again publish nothing.
        provider.apply_usage_limits(update, "2026-01-01T00:00:01.000Z".into()).await;
        assert!(tokio::time::timeout(Duration::from_millis(50), changes.next()).await.is_err());
        // A probe that reports no limits replaces them (only a probe failure keeps them).
        provider.refresh().await;
        assert!(provider.current().usage_limits.is_none());
        scope.close().await;
    }
}
