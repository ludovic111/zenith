//! Port of `Layers/ProviderInstanceRegistryLive.ts`, `ProviderInstanceRegistryHydration.ts` and
//! the registry / mutator services.
//!
//! The registry owns one [`ProviderInstance`] (and its [`InstanceScope`]) per configured
//! instance id, in settings-author order. [`ProviderInstanceRegistry::reconcile`] diffs a fresh
//! config map against the live one: removed and changed entries are closed **before** their
//! replacements are created (at most one live instance per id), unchanged entries keep their
//! instance (same `Arc`), unknown drivers / undecodable configs / failed creates become
//! "unavailable" `ServerProvider` shadows, and one change tick is published per effective
//! batch. [`hydrate`] seeds it from settings and keeps it in sync with every settings change.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, Weak};

use async_trait::async_trait;
use futures::StreamExt;
use zc_contracts::{
    ApprovalRequestId, ModelSelection, ProviderApprovalDecision, ProviderDriverKind, ProviderInstanceConfig, ProviderInstanceId, ProviderRuntimeEvent,
    ProviderSendTurnInput, ProviderSession, ProviderSessionStartInput, ProviderTurnStartResult, ProviderUploadFeedbackInput, ProviderUploadFeedbackResult,
    ProviderUserInputAnswers, ServerProvider, ThreadId, TurnId,
};
use zc_core::{PubSub, Subscription};
use zc_ports::adapter::{AdapterCapabilities, AdapterError, AdapterResult, Compaction, ProviderAdapter, ThreadSnapshot};
use zc_ports::SettingsService;

use crate::driver::{Driver, DriverCreateInput, InstanceScope, ProviderInstance};
use crate::settings::{derive_provider_instance_config_map, resolve_entry_enabled};
use crate::snapshot::build_unavailable_provider_snapshot;

/// A live registry entry: the instance, its scope, the envelope it was built from (to detect
/// no-op updates), and the adapter routing should use (the instance's own, or wrapped by the
/// credential admission guard of `ProviderAdapterRegistry`).
#[derive(Clone)]
struct LiveEntry {
    instance_id: ProviderInstanceId,
    instance: Arc<ProviderInstance>,
    scope: InstanceScope,
    entry: ProviderInstanceConfig,
    routing_adapter: Arc<dyn ProviderAdapter>,
}

struct RegistryInner {
    drivers: Vec<Arc<dyn Driver>>,
    entries: RwLock<Vec<LiveEntry>>,
    unavailable: RwLock<Vec<(ProviderInstanceId, ServerProvider)>>,
    changes: PubSub<()>,
    reconcile_lock: tokio::sync::Mutex<()>,
    closed: AtomicBool,
}

/// `ProviderInstanceRegistry` (+ its mutator).
#[derive(Clone)]
pub struct ProviderInstanceRegistry {
    inner: Arc<RegistryInner>,
}

enum Built {
    Live(Box<LiveEntry>),
    Unavailable(Box<ServerProvider>),
}

impl ProviderInstanceRegistry {
    /// An empty registry over `drivers` (the `BUILT_IN_DRIVERS` list, in presentation order).
    pub fn new(drivers: Vec<Arc<dyn Driver>>) -> Self {
        Self {
            inner: Arc::new(RegistryInner {
                drivers,
                entries: RwLock::new(Vec::new()),
                unavailable: RwLock::new(Vec::new()),
                changes: PubSub::new(),
                reconcile_lock: tokio::sync::Mutex::new(()),
                closed: AtomicBool::new(false),
            }),
        }
    }

    /// `makeProviderInstanceRegistry({drivers, configMap})`: build and hydrate.
    pub async fn with_config(drivers: Vec<Arc<dyn Driver>>, config_map: &[(ProviderInstanceId, ProviderInstanceConfig)]) -> Self {
        let registry = Self::new(drivers);
        registry.reconcile(config_map).await;
        registry
    }

    /// The registered driver kinds, in registration order.
    pub fn driver_kinds(&self) -> Vec<ProviderDriverKind> {
        self.inner.drivers.iter().map(|driver| driver.driver_kind()).collect()
    }

    /// `getInstance(id)`.
    pub fn get_instance(&self, instance_id: &ProviderInstanceId) -> Option<Arc<ProviderInstance>> {
        self.inner
            .entries
            .read()
            .unwrap()
            .iter()
            .find(|entry| &entry.instance_id == instance_id)
            .map(|entry| entry.instance.clone())
    }

    /// The adapter routing uses for `instance_id` (stable identity per instance).
    pub fn get_routing_adapter(&self, instance_id: &ProviderInstanceId) -> Option<Arc<dyn ProviderAdapter>> {
        self.inner
            .entries
            .read()
            .unwrap()
            .iter()
            .find(|entry| &entry.instance_id == instance_id)
            .map(|entry| entry.routing_adapter.clone())
    }

    /// `listInstances`: every live instance, in settings-author order.
    pub fn list_instances(&self) -> Vec<Arc<ProviderInstance>> {
        self.inner.entries.read().unwrap().iter().map(|entry| entry.instance.clone()).collect()
    }

    /// `listUnavailable`: shadow snapshots for unknown drivers and failed instances.
    pub fn list_unavailable(&self) -> Vec<ServerProvider> {
        self.inner.unavailable.read().unwrap().iter().map(|(_, snapshot)| snapshot.clone()).collect()
    }

    /// `subscribeChanges`: one tick per effective reconcile, subscribed when this returns.
    pub fn subscribe_changes(&self) -> Subscription<()> {
        self.inner.changes.subscribe()
    }

    /// `ProviderInstanceRegistryMutator.reconcile(configMap)`. Never fails: bad entries become
    /// unavailable snapshots.
    pub async fn reconcile(&self, config_map: &[(ProviderInstanceId, ProviderInstanceConfig)]) {
        let _guard = self.inner.reconcile_lock.lock().await;
        if self.inner.closed.load(Ordering::SeqCst) {
            return;
        }
        let previous_entries = self.inner.entries.read().unwrap().clone();
        let previous_unavailable = self.inner.unavailable.read().unwrap().clone();
        let next_ids: Vec<&ProviderInstanceId> = config_map.iter().map(|(id, _)| id).collect();

        // 1. Close removed and replaced instances before building their replacements.
        let mut removed = 0usize;
        let mut replaced: Vec<ProviderInstanceId> = Vec::new();
        for live in &previous_entries {
            match config_map.iter().find(|(id, _)| id == &live.instance_id) {
                None => removed += 1,
                Some((_, next)) if *next != live.entry => replaced.push(live.instance_id.clone()),
                Some(_) => {}
            }
        }
        for live in &previous_entries {
            if !next_ids.contains(&&live.instance_id) || replaced.contains(&live.instance_id) {
                live.scope.close().await;
            }
        }

        // 2. Build additions and replacements in settings-author order.
        let mut built_entries: Vec<LiveEntry> = Vec::new();
        let mut built_unavailable: Vec<(ProviderInstanceId, ServerProvider)> = Vec::new();
        for (instance_id, entry) in config_map {
            if let Some(existing) = previous_entries.iter().find(|live| &live.instance_id == instance_id) {
                if !replaced.contains(instance_id) {
                    built_entries.push(existing.clone());
                    continue;
                }
            }
            match self.build_entry(instance_id, entry, &built_entries).await {
                Built::Live(live) => built_entries.push(*live),
                Built::Unavailable(snapshot) => built_unavailable.push((instance_id.clone(), *snapshot)),
            }
        }

        let previous_order: Vec<&ProviderInstanceId> = previous_entries.iter().map(|live| &live.instance_id).collect();
        let order_changed = previous_order != next_ids;
        let entries_changed = order_changed || removed > 0 || !replaced.is_empty() || built_entries.len() != previous_entries.len();
        let unavailable_changed = built_unavailable.len() != previous_unavailable.len()
            || built_unavailable.iter().any(|(id, snapshot)| {
                previous_unavailable
                    .iter()
                    .find(|(previous_id, _)| previous_id == id)
                    .is_none_or(|(_, previous)| previous != snapshot)
            })
            || previous_unavailable
                .iter()
                .any(|(id, _)| !built_unavailable.iter().any(|(next_id, _)| next_id == id));

        *self.inner.entries.write().unwrap() = built_entries;
        *self.inner.unavailable.write().unwrap() = built_unavailable;
        if entries_changed || unavailable_changed {
            self.inner.changes.publish(());
        }
    }

    async fn build_entry(&self, instance_id: &ProviderInstanceId, entry: &ProviderInstanceConfig, built_so_far: &[LiveEntry]) -> Built {
        let unavailable = |reason: String| {
            Built::Unavailable(Box::new(build_unavailable_provider_snapshot(
                &entry.driver,
                instance_id,
                entry.display_name.as_deref(),
                entry.accent_color.as_deref(),
                &reason,
                None,
            )))
        };
        let Some(driver) = self.inner.drivers.iter().find(|driver| driver.driver_kind() == entry.driver).cloned() else {
            return unavailable(format!("Driver '{}' is not registered in this build.", entry.driver));
        };
        if !driver.metadata().supports_multiple_instances && built_so_far.iter().any(|live| live.instance.driver_kind == entry.driver) {
            return unavailable(format!("Driver '{}' supports only one instance.", entry.driver));
        }
        let raw_config = entry.config.clone().unwrap_or_else(|| driver.default_config());
        let typed_config = match driver.decode_config(&raw_config) {
            Ok(config) => config,
            Err(detail) => {
                tracing::error!(instance_id = %instance_id, driver = %entry.driver, %detail, "Failed to decode provider instance config");
                return unavailable(format!("Invalid config for instance '{instance_id}': {detail}"));
            }
        };
        let scope = InstanceScope::new();
        let created = driver
            .create(DriverCreateInput {
                instance_id: instance_id.clone(),
                display_name: entry.display_name.clone(),
                accent_color: entry.accent_color.clone(),
                environment: entry.environment.clone().unwrap_or_default(),
                enabled: resolve_entry_enabled(entry, &typed_config),
                config: typed_config,
                scope: scope.clone(),
            })
            .await;
        match created {
            Ok(instance) => {
                let instance = Arc::new(instance);
                let routing_adapter: Arc<dyn ProviderAdapter> = match &instance.auth {
                    Some(auth) if auth.guards_startup() => Arc::new(GuardedAdapter {
                        instance: Arc::downgrade(&instance),
                        adapter: instance.adapter.clone(),
                        registry: Arc::downgrade(&self.inner),
                    }),
                    _ => instance.adapter.clone(),
                };
                Built::Live(Box::new(LiveEntry {
                    instance_id: instance_id.clone(),
                    instance,
                    scope,
                    entry: entry.clone(),
                    routing_adapter,
                }))
            }
            Err(error) => {
                tracing::error!(instance_id = %instance_id, driver = %entry.driver, detail = %error.detail, "Failed to create provider instance");
                scope.close().await;
                unavailable(format!("Driver '{}' failed to create instance: {}", entry.driver, error.detail))
            }
        }
    }

    /// Close every instance (server shutdown). Later reconciles are ignored.
    pub async fn close(&self) {
        let _guard = self.inner.reconcile_lock.lock().await;
        self.inner.closed.store(true, Ordering::SeqCst);
        let entries: Vec<LiveEntry> = std::mem::take(&mut *self.inner.entries.write().unwrap());
        for live in entries.iter().rev() {
            live.scope.close().await;
        }
        self.inner.changes.shutdown();
    }
}

/// `ProviderInstanceRegistryHydrationLive`: subscribe to settings changes, seed the registry from
/// the current settings (`deriveProviderInstanceConfigMap`), then reconcile on every change.
/// Returns the registry and the watcher task (abort it, then [`ProviderInstanceRegistry::close`],
/// on shutdown).
pub async fn hydrate(drivers: Vec<Arc<dyn Driver>>, settings: Arc<dyn SettingsService>) -> (ProviderInstanceRegistry, tokio::task::JoinHandle<()>) {
    let registry = ProviderInstanceRegistry::new(drivers);
    let kinds = registry.driver_kinds();
    let mut changes = settings.subscribe_changes();
    let initial = match settings.get_settings().await {
        Ok(value) => derive_provider_instance_config_map(&serde_json::to_value(&value).unwrap_or_default(), &kinds),
        Err(error) => {
            tracing::warn!(?error, "could not read settings; starting with no provider instances");
            Vec::new()
        }
    };
    registry.reconcile(&initial).await;
    let watcher_registry = registry.clone();
    let watcher = tokio::spawn(async move {
        while let Some(next) = changes.next().await {
            watcher_registry
                .reconcile(&derive_provider_instance_config_map(&serde_json::to_value(&next).unwrap_or_default(), &kinds))
                .await;
        }
    });
    (registry, watcher)
}

/// `ProviderAdapterRegistryLive`'s guard: session startup on an instance with a credential
/// controller waits for the shared admission of every instance using the same credentials and
/// is refused while a sign-in change is in progress.
struct GuardedAdapter {
    instance: Weak<ProviderInstance>,
    adapter: Arc<dyn ProviderAdapter>,
    registry: Weak<RegistryInner>,
}

#[async_trait]
impl ProviderAdapter for GuardedAdapter {
    fn provider(&self) -> ProviderDriverKind {
        self.adapter.provider()
    }
    fn capabilities(&self) -> AdapterCapabilities {
        self.adapter.capabilities()
    }
    fn compaction(&self) -> Option<Compaction> {
        self.adapter.compaction()
    }
    async fn start_session(&self, input: ProviderSessionStartInput) -> AdapterResult<ProviderSession> {
        let Some(instance) = self.instance.upgrade() else {
            return self.adapter.start_session(input).await;
        };
        let refuse = |detail: &str| AdapterError::Validation {
            provider: instance.driver_kind.to_string(),
            operation: "startSession".into(),
            issue: detail.to_owned(),
        };
        let binding = instance.auth.as_ref().and_then(|auth| auth.credential_binding());
        let related: Vec<Arc<ProviderInstance>> = match (&binding, self.registry.upgrade()) {
            (Some(binding), Some(registry)) => registry
                .entries
                .read()
                .unwrap()
                .iter()
                .filter(|live| live.instance.auth.as_ref().and_then(|auth| auth.credential_binding()).as_ref() == Some(binding))
                .map(|live| live.instance.clone())
                .collect(),
            _ => vec![instance.clone()],
        };
        for peer in &related {
            if let Some(auth) = &peer.auth {
                if auth.is_changing_credentials().await {
                    return Err(refuse("Provider sign-in is changing. Try again after it finishes."));
                }
            }
        }
        let mut guards = Vec::new();
        for peer in &related {
            if let Some(auth) = &peer.auth {
                match auth.begin_access().await {
                    Ok(guard) => guards.push(guard),
                    Err(detail) => return Err(refuse(&detail)),
                }
            }
        }
        let result = self.adapter.start_session(input).await;
        drop(guards);
        result
    }
    async fn send_turn(&self, input: ProviderSendTurnInput) -> AdapterResult<ProviderTurnStartResult> {
        self.adapter.send_turn(input).await
    }
    async fn start_compaction(&self, thread_id: &ThreadId, model_selection: Option<ModelSelection>) -> AdapterResult<()> {
        self.adapter.start_compaction(thread_id, model_selection).await
    }
    async fn interrupt_turn(&self, thread_id: &ThreadId, turn_id: Option<&TurnId>) -> AdapterResult<()> {
        self.adapter.interrupt_turn(thread_id, turn_id).await
    }
    async fn respond_to_request(&self, thread_id: &ThreadId, request_id: &ApprovalRequestId, decision: ProviderApprovalDecision) -> AdapterResult<()> {
        self.adapter.respond_to_request(thread_id, request_id, decision).await
    }
    async fn respond_to_user_input(&self, thread_id: &ThreadId, request_id: &ApprovalRequestId, answers: ProviderUserInputAnswers) -> AdapterResult<()> {
        self.adapter.respond_to_user_input(thread_id, request_id, answers).await
    }
    async fn stop_session(&self, thread_id: &ThreadId) -> AdapterResult<()> {
        self.adapter.stop_session(thread_id).await
    }
    async fn list_sessions(&self) -> Vec<ProviderSession> {
        self.adapter.list_sessions().await
    }
    async fn has_session(&self, thread_id: &ThreadId) -> bool {
        self.adapter.has_session(thread_id).await
    }
    async fn read_thread(&self, thread_id: &ThreadId) -> AdapterResult<ThreadSnapshot> {
        self.adapter.read_thread(thread_id).await
    }
    async fn rollback_thread(&self, thread_id: &ThreadId, num_turns: u32) -> AdapterResult<ThreadSnapshot> {
        self.adapter.rollback_thread(thread_id, num_turns).await
    }
    async fn upload_feedback(&self, input: ProviderUploadFeedbackInput) -> Option<AdapterResult<ProviderUploadFeedbackResult>> {
        self.adapter.upload_feedback(input).await
    }
    async fn stop_all(&self) -> AdapterResult<()> {
        self.adapter.stop_all().await
    }
    fn subscribe_events(&self) -> futures::stream::BoxStream<'static, ProviderRuntimeEvent> {
        self.adapter.subscribe_events()
    }
}
