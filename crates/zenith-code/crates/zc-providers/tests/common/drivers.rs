//! Fake drivers: a [`Driver`] whose instances hold a [`FakeAdapter`] and a
//! [`ManagedServerProvider`] over a scripted status probe.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use zc_contracts::{ProviderDriverKind, ServerProvider, ServerProviderAuthStatus, ServerProviderState};
use zc_providers::driver::{ContinuationIdentity, Driver, DriverCreateInput, DriverMetadata, ProviderInstance};
use zc_providers::managed::{EnrichmentPublisher, ManagedProviderProbe, ManagedServerProvider, ManagedServerProviderOptions};
use zc_providers::snapshot::{
    build_server_provider, with_instance_identity, BuildServerProviderInput, InstanceIdentity, ProviderMaintenanceCapabilities, ProviderProbeResult,
    ServerProviderPresentation,
};
use zc_providers::ProviderDriverError;

use super::FakeAdapter;

/// How a fake probe answers.
#[derive(Clone)]
pub struct ProbeScript {
    pub installed: bool,
    pub version: Option<String>,
    pub status: ServerProviderState,
    pub auth: ServerProviderAuthStatus,
    pub models: Vec<Value>,
    pub fail: bool,
}

impl Default for ProbeScript {
    fn default() -> Self {
        Self {
            installed: true,
            version: Some("1.0.0".into()),
            status: ServerProviderState::Ready,
            auth: ServerProviderAuthStatus::Authenticated,
            models: vec![json!({"slug": "model-a", "name": "Model A", "isCustom": false, "capabilities": null})],
            fail: false,
        }
    }
}

/// A status probe over a fake CLI: counts calls, answers from a script.
pub struct FakeProbe {
    pub identity: InstanceIdentity,
    pub enabled: bool,
    pub settings: Mutex<Value>,
    pub script: Mutex<ProbeScript>,
    pub checks: AtomicUsize,
    pub gate: Option<Arc<tokio::sync::Semaphore>>,
    pub enrichment: Mutex<Option<String>>,
    pub probe_on_settings_change: bool,
}

impl FakeProbe {
    pub fn snapshot(&self, probe: ProviderProbeResult, models: Vec<Value>) -> ServerProvider {
        let draft = build_server_provider(BuildServerProviderInput {
            driver: Some(self.identity.driver_kind.clone()),
            presentation: ServerProviderPresentation {
                display_name: "Fake".into(),
                ..Default::default()
            },
            enabled: self.enabled,
            checked_at: zc_core::now_iso(),
            models: models.into_iter().map(|model| serde_json::from_value(model).unwrap()).collect(),
            slash_commands: Vec::new(),
            skills: Vec::new(),
            probe,
        });
        with_instance_identity(&self.identity, draft)
    }
}

#[async_trait]
impl ManagedProviderProbe for FakeProbe {
    type Settings = Value;

    async fn get_settings(&self) -> Result<Value, String> {
        Ok(self.settings.lock().unwrap().clone())
    }

    fn have_settings_changed(&self, previous: &Value, next: &Value) -> bool {
        previous != next
    }

    async fn initial_snapshot(&self, _settings: &Value) -> ServerProvider {
        self.snapshot(
            ProviderProbeResult {
                installed: false,
                version: None,
                status: ServerProviderState::Warning,
                auth: ProviderProbeResult::auth(ServerProviderAuthStatus::Unknown),
                message: Some("Checking provider status...".into()),
                usage_limits: None,
            },
            Vec::new(),
        )
    }

    async fn check_provider(&self) -> Result<ServerProvider, String> {
        if let Some(gate) = &self.gate {
            gate.acquire().await.map_err(|error| error.to_string())?.forget();
        }
        self.checks.fetch_add(1, Ordering::SeqCst);
        let script = self.script.lock().unwrap().clone();
        if script.fail {
            return Ok(self.snapshot(
                ProviderProbeResult {
                    installed: true,
                    version: script.version,
                    status: ServerProviderState::Error,
                    auth: ProviderProbeResult::auth(script.auth),
                    message: Some("probe failed".into()),
                    usage_limits: None,
                },
                Vec::new(),
            ));
        }
        Ok(self.snapshot(
            ProviderProbeResult {
                installed: script.installed,
                version: script.version,
                status: script.status,
                auth: ProviderProbeResult::auth(script.auth),
                message: None,
                usage_limits: None,
            },
            script.models,
        ))
    }

    fn check_provider_on_settings_change(&self, _previous: &Value, _next: &Value) -> bool {
        self.probe_on_settings_change
    }

    fn has_enrichment(&self) -> bool {
        self.enrichment.lock().unwrap().is_some()
    }

    async fn enrich_snapshot(&self, _settings: Value, snapshot: ServerProvider, publisher: EnrichmentPublisher) {
        let message = self.enrichment.lock().unwrap().clone();
        if let Some(message) = message {
            let mut enriched = snapshot;
            enriched.message = Some(message);
            publisher.publish(enriched);
        }
    }

    async fn resolve_maintenance(&self, _fresh: bool) -> ProviderMaintenanceCapabilities {
        ProviderMaintenanceCapabilities::manual_only(self.identity.driver_kind.clone(), None)
    }
}

/// One created instance, as the test sees it.
pub struct Created {
    pub instance_id: String,
    pub config: Value,
    pub enabled: bool,
    pub adapter: Arc<FakeAdapter>,
    pub probe: Arc<FakeProbe>,
    pub scope: zc_providers::InstanceScope,
}

/// A configurable fake driver.
pub struct FakeDriver {
    pub kind: String,
    pub multiple: bool,
    pub created: Mutex<Vec<Created>>,
    pub fail_create: Mutex<bool>,
    pub script: Mutex<ProbeScript>,
    pub gate: Option<Arc<tokio::sync::Semaphore>>,
    pub refresh_interval_ms: Option<i64>,
    /// Instances answer `snapshot_for_cwd` with one skill named after the cwd.
    pub cwd_snapshots: bool,
    pub cwd_calls: Arc<AtomicUsize>,
}

impl FakeDriver {
    pub fn new(kind: &str) -> Arc<Self> {
        Arc::new(Self {
            kind: kind.into(),
            multiple: true,
            created: Mutex::new(Vec::new()),
            fail_create: Mutex::new(false),
            script: Mutex::new(ProbeScript::default()),
            gate: None,
            refresh_interval_ms: Some(0),
            cwd_snapshots: false,
            cwd_calls: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn with_cwd_snapshots(kind: &str) -> Arc<Self> {
        Arc::new(Self {
            kind: kind.into(),
            multiple: true,
            created: Mutex::new(Vec::new()),
            fail_create: Mutex::new(false),
            script: Mutex::new(ProbeScript::default()),
            gate: None,
            refresh_interval_ms: Some(0),
            cwd_snapshots: true,
            cwd_calls: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn gated(kind: &str, gate: Arc<tokio::sync::Semaphore>) -> Arc<Self> {
        Arc::new(Self {
            kind: kind.into(),
            multiple: true,
            created: Mutex::new(Vec::new()),
            fail_create: Mutex::new(false),
            script: Mutex::new(ProbeScript::default()),
            gate: Some(gate),
            refresh_interval_ms: Some(0),
            cwd_snapshots: false,
            cwd_calls: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn created_ids(&self) -> Vec<String> {
        self.created.lock().unwrap().iter().map(|created| created.instance_id.clone()).collect()
    }

    pub fn last(&self, instance_id: &str) -> (Arc<FakeAdapter>, Arc<FakeProbe>, zc_providers::InstanceScope) {
        let created = self.created.lock().unwrap();
        let entry = created
            .iter()
            .rev()
            .find(|created| created.instance_id == instance_id)
            .expect("instance created");
        (entry.adapter.clone(), entry.probe.clone(), entry.scope.clone())
    }
}

#[async_trait]
impl Driver for FakeDriver {
    fn driver_kind(&self) -> ProviderDriverKind {
        ProviderDriverKind::from(self.kind.as_str())
    }

    fn metadata(&self) -> DriverMetadata {
        DriverMetadata {
            display_name: format!("Fake {}", self.kind),
            supports_multiple_instances: self.multiple,
        }
    }

    fn decode_config(&self, raw: &Value) -> Result<Value, String> {
        let object = raw.as_object().ok_or_else(|| "Expected an object".to_owned())?;
        if let Some(path) = object.get("binaryPath") {
            if !path.is_string() {
                return Err("Expected string at [\"binaryPath\"]".into());
            }
        }
        let mut decoded = json!({"enabled": true, "binaryPath": self.kind});
        for (key, value) in object {
            decoded[key] = value.clone();
        }
        Ok(decoded)
    }

    fn default_config(&self) -> Value {
        json!({})
    }

    async fn create(&self, input: DriverCreateInput) -> Result<ProviderInstance, ProviderDriverError> {
        if *self.fail_create.lock().unwrap() {
            return Err(ProviderDriverError::new(&self.kind, input.instance_id.as_str(), "binary exploded"));
        }
        let driver_kind = self.driver_kind();
        let identity = InstanceIdentity {
            instance_id: input.instance_id.clone(),
            driver_kind: driver_kind.clone(),
            display_name: input.display_name.clone(),
            accent_color: input.accent_color.clone(),
            continuation_group_key: format!("{}:instance:{}", driver_kind, input.instance_id),
        };
        let probe = Arc::new(FakeProbe {
            identity,
            enabled: input.enabled,
            settings: Mutex::new(input.config.clone()),
            script: Mutex::new(self.script.lock().unwrap().clone()),
            checks: AtomicUsize::new(0),
            gate: self.gate.clone(),
            enrichment: Mutex::new(None),
            probe_on_settings_change: true,
        });
        let mut options = ManagedServerProviderOptions::new(input.scope.clone());
        options.refresh_interval_ms = self.refresh_interval_ms;
        let snapshot = ManagedServerProvider::start(probe.clone(), futures::stream::empty().boxed(), options)
            .await
            .map_err(|detail| ProviderDriverError::new(&self.kind, input.instance_id.as_str(), detail))?;
        let adapter = FakeAdapter::new(&self.kind);
        let scope = input.scope.clone();
        {
            let adapter = adapter.clone();
            scope.add_finalizer(async move {
                let _ = zc_ports::adapter::ProviderAdapter::stop_all(&*adapter).await;
            });
        }
        self.created.lock().unwrap().push(Created {
            instance_id: input.instance_id.to_string(),
            config: input.config.clone(),
            enabled: input.enabled,
            adapter: adapter.clone(),
            probe: probe.clone(),
            scope,
        });
        Ok(ProviderInstance {
            continuation_identity: ContinuationIdentity::default_for(&driver_kind, &input.instance_id),
            instance_id: input.instance_id,
            driver_kind,
            display_name: input.display_name,
            accent_color: input.accent_color,
            enabled: input.enabled,
            snapshot_for_cwd: self.cwd_snapshots.then(|| {
                let snapshot = snapshot.clone();
                let calls = self.cwd_calls.clone();
                let hook: zc_providers::driver::SnapshotForCwd = Arc::new(move |cwd: String| {
                    let snapshot = snapshot.clone();
                    let calls = calls.clone();
                    Box::pin(async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        let mut scoped = snapshot.current();
                        scoped.skills =
                            vec![serde_json::from_value(json!({"name": format!("skill{}", cwd.replace('/', "-")), "path": cwd, "enabled": true})).unwrap()];
                        Ok(scoped)
                    })
                });
                hook
            }),
            snapshot: Arc::new(snapshot),
            refresh_models: None,
            invalidate_caches: None,
            consume_reset_credit: None,
            adapter,
            text_generation: None,
            auth: None,
        })
    }
}
