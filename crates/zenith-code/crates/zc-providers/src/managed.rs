//! Port of `provider/makeManagedServerProvider.ts`: the snapshot half every driver builds.
//!
//! A [`ManagedServerProvider`] publishes an initial (pending) snapshot at once, probes the
//! provider in the background, re-probes when the driver's settings change (or only re-enriches
//! when [`ManagedProviderProbe::check_provider_on_settings_change`] says so), refreshes on the
//! provider health interval while some client wants provider status, and folds runtime usage
//! updates in between probes. Enrichment (e.g. a version advisory that needs the network) runs
//! after each probe and is dropped when a newer probe or settings change advanced the generation.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use zc_contracts::{ProviderUsageLimitsUpdate, ServerProvider, ServerProviderUsageLimits};
use zc_core::PubSub;
use zc_ports::contracts::BackgroundScope;
use zc_ports::{BackgroundPolicy, EventStream, SettingsService};

use crate::driver::{InstanceScope, ServerProviderSource};
use crate::settings::provider_health_refresh_interval_ms;
use crate::snapshot::ProviderMaintenanceCapabilities;
use crate::usage_limits::{apply_usage_limits_update, resolve_usage_limits_after_probe};

/// What a driver plugs into [`ManagedServerProvider`].
#[async_trait]
pub trait ManagedProviderProbe: Send + Sync + 'static {
    /// The driver's slice of settings (`ProviderSnapshotSettings<…>` in TS).
    type Settings: Clone + Send + Sync + 'static;

    /// `getSettings`.
    async fn get_settings(&self) -> Result<Self::Settings, String>;
    /// `haveSettingsChanged(previous, next)`.
    fn have_settings_changed(&self, previous: &Self::Settings, next: &Self::Settings) -> bool;
    /// `initialSnapshot(settings)`: the pending snapshot shown before the first probe.
    async fn initial_snapshot(&self, settings: &Self::Settings) -> ServerProvider;
    /// `checkProvider`: the status probe (fake CLIs in tests; never a real turn).
    async fn check_provider(&self) -> Result<ServerProvider, String>;
    /// `checkProviderOnSettingsChange(previous, next)`: `false` skips the probe and only
    /// re-runs enrichment.
    fn check_provider_on_settings_change(&self, _previous: &Self::Settings, _next: &Self::Settings) -> bool {
        true
    }
    /// Whether [`Self::enrich_snapshot`] does anything (TS: `enrichSnapshot` present).
    fn has_enrichment(&self) -> bool {
        false
    }
    /// `enrichSnapshot`: publish supplemental snapshots through `publisher`.
    async fn enrich_snapshot(&self, _settings: Self::Settings, _snapshot: ServerProvider, _publisher: EnrichmentPublisher) {}
    /// `resolveMaintenance({fresh})`.
    async fn resolve_maintenance(&self, fresh: bool) -> ProviderMaintenanceCapabilities;
}

/// Options of [`ManagedServerProvider::start`].
#[derive(Clone)]
pub struct ManagedServerProviderOptions {
    /// Fixed refresh interval; `None` follows the server's provider health interval setting.
    pub refresh_interval_ms: Option<i64>,
    /// `refreshOnInterval` (default true).
    pub refresh_on_interval: bool,
    pub background_policy: Option<Arc<dyn BackgroundPolicy>>,
    pub server_settings: Option<Arc<dyn SettingsService>>,
    pub scope: InstanceScope,
}

impl ManagedServerProviderOptions {
    pub fn new(scope: InstanceScope) -> Self {
        Self {
            refresh_interval_ms: None,
            refresh_on_interval: true,
            background_policy: None,
            server_settings: None,
            scope,
        }
    }
}

struct SnapshotState {
    snapshot: ServerProvider,
    enrichment_generation: u64,
}

struct ManagedInner<P: ManagedProviderProbe> {
    probe: Arc<P>,
    state: Mutex<SnapshotState>,
    settings: Mutex<P::Settings>,
    changes: PubSub<ServerProvider>,
    refresh_lock: tokio::sync::Mutex<()>,
    enrichment: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Publishes enriched snapshots for one generation; stale publishes are ignored.
#[derive(Clone)]
pub struct EnrichmentPublisher {
    generation: u64,
    publish: Arc<dyn Fn(u64, ServerProvider) + Send + Sync>,
    current: Arc<dyn Fn() -> ServerProvider + Send + Sync>,
}

impl EnrichmentPublisher {
    /// `publishSnapshot(snapshot)`.
    pub fn publish(&self, snapshot: ServerProvider) {
        (self.publish)(self.generation, snapshot);
    }

    /// `getSnapshot`.
    pub fn current(&self) -> ServerProvider {
        (self.current)()
    }
}

fn with_usage_limits(mut snapshot: ServerProvider, usage_limits: Option<ServerProviderUsageLimits>) -> ServerProvider {
    snapshot.usage_limits = usage_limits;
    snapshot
}

/// `makeManagedServerProvider(input)`.
pub struct ManagedServerProvider<P: ManagedProviderProbe> {
    inner: Arc<ManagedInner<P>>,
}

impl<P: ManagedProviderProbe> Clone for ManagedServerProvider<P> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

impl<P: ManagedProviderProbe> ManagedServerProvider<P> {
    /// Build it, start the settings watcher, the refresh loop and the startup probe (all owned
    /// by `options.scope`). Fails only when the initial settings cannot be read.
    pub async fn start(probe: Arc<P>, settings_changes: EventStream<P::Settings>, options: ManagedServerProviderOptions) -> Result<Self, String> {
        let initial_settings = probe.get_settings().await?;
        let initial_snapshot = probe.initial_snapshot(&initial_settings).await;
        let changes = PubSub::new();
        let inner = Arc::new(ManagedInner {
            probe,
            state: Mutex::new(SnapshotState {
                snapshot: initial_snapshot,
                enrichment_generation: 0,
            }),
            settings: Mutex::new(initial_settings.clone()),
            changes: changes.clone(),
            refresh_lock: tokio::sync::Mutex::new(()),
            enrichment: Mutex::new(None),
        });
        let provider = Self { inner };
        let scope = options.scope.clone();
        scope.add_finalizer({
            let changes = changes.clone();
            let provider = provider.clone();
            async move {
                if let Some(task) = provider.inner.enrichment.lock().unwrap().take() {
                    task.abort();
                }
                changes.shutdown();
            }
        });

        // Settings changes re-probe (or re-enrich).
        {
            let provider = provider.clone();
            let mut settings_changes = settings_changes;
            scope.spawn(async move {
                while let Some(next) = settings_changes.next().await {
                    provider.apply_snapshot(next, false).await;
                }
            });
        }

        // The refresh loop, woken early when the configured interval changes.
        let interval_changed = Arc::new(tokio::sync::Notify::new());
        if options.refresh_interval_ms.is_none() {
            if let Some(server_settings) = &options.server_settings {
                let mut stream = server_settings.subscribe_changes();
                let notify = interval_changed.clone();
                let mut last = server_settings
                    .get_settings()
                    .await
                    .ok()
                    .map(|settings| provider_health_refresh_interval_ms(&serde_json::to_value(&settings).unwrap_or_default()));
                scope.spawn(async move {
                    while let Some(settings) = stream.next().await {
                        let next = provider_health_refresh_interval_ms(&serde_json::to_value(&settings).unwrap_or_default());
                        if last != Some(next) {
                            last = Some(next);
                            notify.notify_one();
                        }
                    }
                });
            }
        }
        {
            let provider = provider.clone();
            let options = options.clone();
            scope.spawn(async move {
                loop {
                    let interval = match (options.refresh_interval_ms, &options.server_settings) {
                        (Some(interval), _) => interval,
                        (None, Some(settings)) => match settings.get_settings().await {
                            Ok(value) => provider_health_refresh_interval_ms(&serde_json::to_value(&value).unwrap_or_default()),
                            Err(_) => crate::settings::DEFAULT_PROVIDER_HEALTH_REFRESH_INTERVAL_MS,
                        },
                        (None, None) => crate::settings::DEFAULT_PROVIDER_HEALTH_REFRESH_INTERVAL_MS,
                    };
                    let sleep_ms = if interval <= 0 { 60_000 } else { interval as u64 };
                    let elapsed = tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(sleep_ms)) => true,
                        _ = interval_changed.notified() => false,
                    };
                    if options.refresh_on_interval && elapsed && interval > 0 && provider.has_provider_status_demand(options.background_policy.as_deref()).await
                    {
                        provider.refresh_snapshot().await;
                    }
                }
            });
        }

        // The startup probe never blocks construction.
        {
            let provider = provider.clone();
            scope.spawn(async move {
                provider.apply_snapshot(initial_settings, true).await;
            });
        }
        Ok(provider)
    }

    async fn has_provider_status_demand(&self, policy: Option<&dyn BackgroundPolicy>) -> bool {
        let Some(policy) = policy else {
            return true;
        };
        let instance_id = self.inner.state.lock().unwrap().snapshot.instance_id.to_string();
        policy.should_run_scope_work(&BackgroundScope::ProviderStatus { instance_id: None }).await
            || policy
                .should_run_scope_work(&BackgroundScope::ProviderStatus {
                    instance_id: Some(instance_id.into()),
                })
                .await
    }

    fn publish_enriched(inner: &Arc<ManagedInner<P>>, generation: u64, next: ServerProvider) {
        let to_publish = {
            let mut state = inner.state.lock().unwrap();
            if state.enrichment_generation != generation {
                return;
            }
            // A runtime usage update that landed since must not be reverted.
            let merged = with_usage_limits(next, state.snapshot.usage_limits.clone());
            if state.snapshot == merged {
                return;
            }
            state.snapshot = merged.clone();
            merged
        };
        inner.changes.publish(to_publish);
    }

    fn restart_enrichment(&self, settings: P::Settings, snapshot: ServerProvider, generation: u64) {
        if let Some(previous) = self.inner.enrichment.lock().unwrap().take() {
            previous.abort();
        }
        if !self.inner.probe.has_enrichment() {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        let publish_weak = weak.clone();
        let publisher = EnrichmentPublisher {
            generation,
            publish: Arc::new(move |generation, snapshot| {
                if let Some(inner) = publish_weak.upgrade() {
                    Self::publish_enriched(&inner, generation, snapshot);
                }
            }),
            current: Arc::new(move || {
                weak.upgrade()
                    .map(|inner| inner.state.lock().unwrap().snapshot.clone())
                    .unwrap_or_else(snapshot_placeholder)
            }),
        };
        let probe = self.inner.probe.clone();
        let task = tokio::spawn(async move { probe.enrich_snapshot(settings, snapshot, publisher).await });
        *self.inner.enrichment.lock().unwrap() = Some(task);
    }

    /// `applySnapshot(nextSettings, {forceRefresh})`.
    async fn apply_snapshot(&self, next_settings: P::Settings, force: bool) -> ServerProvider {
        let _guard = self.inner.refresh_lock.lock().await;
        let previous_settings = self.inner.settings.lock().unwrap().clone();
        if !force && !self.inner.probe.have_settings_changed(&previous_settings, &next_settings) {
            *self.inner.settings.lock().unwrap() = next_settings;
            return self.current();
        }
        if !force && !self.inner.probe.check_provider_on_settings_change(&previous_settings, &next_settings) {
            let (snapshot, generation) = {
                let mut state = self.inner.state.lock().unwrap();
                state.enrichment_generation += 1;
                (state.snapshot.clone(), state.enrichment_generation)
            };
            *self.inner.settings.lock().unwrap() = next_settings.clone();
            self.restart_enrichment(next_settings, snapshot.clone(), generation);
            return snapshot;
        }
        let probed = match self.inner.probe.check_provider().await {
            Ok(probed) => probed,
            Err(error) => {
                tracing::error!(%error, "provider status check failed");
                return self.current();
            }
        };
        let (snapshot, generation) = {
            let mut state = self.inner.state.lock().unwrap();
            let generation = if self.inner.probe.has_enrichment() {
                state.enrichment_generation + 1
            } else {
                state.enrichment_generation
            };
            let usage_limits = resolve_usage_limits_after_probe(state.snapshot.usage_limits.as_ref(), probed.usage_limits.as_ref());
            let snapshot = with_usage_limits(probed, usage_limits);
            state.snapshot = snapshot.clone();
            state.enrichment_generation = generation;
            (snapshot, generation)
        };
        *self.inner.settings.lock().unwrap() = next_settings.clone();
        self.inner.changes.publish(snapshot.clone());
        self.restart_enrichment(next_settings, snapshot.clone(), generation);
        snapshot
    }

    /// `refreshSnapshot()`: re-read the settings and probe now.
    pub async fn refresh_snapshot(&self) -> ServerProvider {
        match self.inner.probe.get_settings().await {
            Ok(settings) => self.apply_snapshot(settings, true).await,
            Err(error) => {
                tracing::error!(%error, "provider settings unavailable for refresh");
                self.current()
            }
        }
    }

    /// The published snapshot.
    pub fn current(&self) -> ServerProvider {
        self.inner.state.lock().unwrap().snapshot.clone()
    }

    /// `applyUsageLimits(update)`: only `usageLimits` changes; the enrichment generation stays.
    pub fn apply_usage_limits_now(&self, update: &ProviderUsageLimitsUpdate, checked_at: &str) {
        let to_publish = {
            let mut state = self.inner.state.lock().unwrap();
            let Some(usage_limits) = apply_usage_limits_update(state.snapshot.usage_limits.as_ref(), update, checked_at) else {
                return;
            };
            state.snapshot.usage_limits = Some(usage_limits);
            state.snapshot.clone()
        };
        self.inner.changes.publish(to_publish);
    }
}

fn snapshot_placeholder() -> ServerProvider {
    crate::snapshot::build_unavailable_provider_snapshot(&"unknown".into(), &"unknown".into(), None, None, "closed", None)
}

#[async_trait]
impl<P: ManagedProviderProbe> ServerProviderSource for ManagedServerProvider<P> {
    async fn get_snapshot(&self) -> ServerProvider {
        self.current()
    }

    async fn refresh(&self) -> ServerProvider {
        self.refresh_snapshot().await
    }

    fn subscribe_changes(&self) -> EventStream<ServerProvider> {
        self.inner.changes.subscribe().boxed()
    }

    async fn resolve_maintenance(&self, fresh: bool) -> ProviderMaintenanceCapabilities {
        self.inner.probe.resolve_maintenance(fresh).await
    }

    async fn apply_usage_limits(&self, update: ProviderUsageLimitsUpdate, checked_at: String) {
        self.apply_usage_limits_now(&update, &checked_at);
    }
}
