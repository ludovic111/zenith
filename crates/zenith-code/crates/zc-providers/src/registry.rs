//! Port of `Layers/ProviderRegistry.ts`: aggregates every instance's status snapshot into one
//! ordered list (`server.getConfig().providers`, `subscribeServerConfig.providerStatuses`).
//!
//! Boot shows cached (`caches/<instanceId>.json`) or pending snapshots at once; live probe
//! results arrive through each instance's change stream, which is subscribed before its current
//! snapshot is read so no probe result is lost. Unavailable instances are merged in as shadows.
//! Snapshots are merged with the previous one (models a failed probe did not report are kept,
//! capabilities filled in), classified against the model manifest's compatibility policies,
//! persisted, and published only when the list actually changed.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use tokio_util::sync::CancellationToken;
use zc_contracts::{
    ProviderDriverKind, ProviderInstanceId, ServerProvider, ServerProviderAuthStatus, ServerProviderModel, ServerProviderState, ServerProviderUpdateState,
    ServerProviderUpdateStatus, ServerProviderWorkspaceSnapshot,
};
use zc_core::{PubSub, Subscription};

use crate::compatibility::apply_provider_compatibility;
use crate::driver::ProviderInstance;
use crate::instance_registry::ProviderInstanceRegistry;
use crate::manifest::{bundled_model_manifest, ModelManifest, ModelManifestData};
use crate::snapshot::ProviderMaintenanceCapabilities;
use crate::status_cache::{
    hydrate_cached_provider, is_cached_provider_correlated, order_provider_snapshots, read_provider_status_cache, resolve_provider_status_cache_path,
    write_provider_status_cache,
};

const MAX_WORKSPACE_SNAPSHOTS_PER_PROVIDER: usize = 16;

fn has_model_capabilities(model: &ServerProviderModel) -> bool {
    model
        .capabilities
        .as_ref()
        .and_then(|capabilities| capabilities.option_descriptors.as_ref())
        .is_some_and(|descriptors| !descriptors.is_empty())
}

/// `upsertProviderWorkspaceSnapshot`.
pub fn upsert_provider_workspace_snapshot(provider: &ServerProvider, cwd: &str, scoped: &ServerProvider) -> ServerProvider {
    let mut snapshots: Vec<ServerProviderWorkspaceSnapshot> = provider
        .workspace_snapshots
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|snapshot| snapshot.cwd != cwd)
        .collect();
    snapshots.push(ServerProviderWorkspaceSnapshot {
        cwd: cwd.to_owned(),
        checked_at: scoped.checked_at.clone(),
        slash_commands: scoped.slash_commands.clone(),
        skills: scoped.skills.clone(),
    });
    let start = snapshots.len().saturating_sub(MAX_WORKSPACE_SNAPSHOTS_PER_PROVIDER);
    let mut next = provider.clone();
    next.workspace_snapshots = Some(snapshots.split_off(start));
    next
}

fn should_retain_missing_provider_models(provider: &ServerProvider) -> bool {
    let driver = provider.driver.as_str();
    let is_antigravity = driver == "antigravity";
    let is_codex = driver == "codex";
    if !is_antigravity && !is_codex && driver != "opencode" {
        return true;
    }
    if (is_antigravity || is_codex) && (!provider.enabled || provider.auth.status == ServerProviderAuthStatus::Unauthenticated) {
        return false;
    }
    let pending_antigravity_authentication =
        is_antigravity && provider.status == ServerProviderState::Warning && provider.auth.status == ServerProviderAuthStatus::Unknown;
    let pending_initial_probe = provider.enabled && !provider.installed && provider.status == ServerProviderState::Warning;
    let installed_probe_failed = provider.installed && provider.status == ServerProviderState::Error;
    pending_antigravity_authentication || pending_initial_probe || installed_probe_failed
}

fn should_retain_missing_opencode_metadata(provider: &ServerProvider) -> bool {
    provider.driver.as_str() == "opencode" && should_retain_missing_provider_models(provider)
}

fn merge_provider_models(provider: &ServerProvider, previous: &[ServerProviderModel], next: &[ServerProviderModel]) -> Vec<ServerProviderModel> {
    let retain = should_retain_missing_provider_models(provider);
    // Custom rows come from settings and every snapshot carries the full list.
    let retainable: Vec<&ServerProviderModel> = previous.iter().filter(|model| !model.is_custom).collect();
    if retain && next.is_empty() && !retainable.is_empty() {
        return retainable.into_iter().cloned().collect();
    }
    let mut merged: Vec<ServerProviderModel> = next
        .iter()
        .map(|model| match previous.iter().find(|candidate| candidate.slug == model.slug) {
            Some(previous_model) if !has_model_capabilities(model) && has_model_capabilities(previous_model) => {
                let mut filled = model.clone();
                filled.capabilities = previous_model.capabilities.clone();
                filled
            }
            _ => model.clone(),
        })
        .collect();
    if retain {
        let next_slugs: HashSet<&str> = next.iter().map(|model| model.slug.as_str()).collect();
        merged.extend(retainable.into_iter().filter(|model| !next_slugs.contains(model.slug.as_str())).cloned());
    }
    merged
}

/// `carrySavedAntigravityAccount`.
fn carry_saved_antigravity_account(previous: &ServerProvider, next: &ServerProvider) -> Option<(zc_contracts::ServerProviderAuth, ServerProviderState)> {
    if next.driver.as_str() != "antigravity"
        || previous.driver.as_str() != "antigravity"
        || !next.enabled
        || next.auth.status != ServerProviderAuthStatus::Unknown
        || previous.auth.status != ServerProviderAuthStatus::Authenticated
        || (next.auth.r#type.is_some() && next.auth.r#type != previous.auth.r#type)
        || (!next.installed && next.status != ServerProviderState::Warning)
    {
        return None;
    }
    let status = if next.installed && next.status == ServerProviderState::Warning {
        ServerProviderState::Ready
    } else {
        next.status
    };
    Some((previous.auth.clone(), status))
}

/// `mergeProviderSnapshot(previous, next)`.
pub fn merge_provider_snapshot(previous: Option<&ServerProvider>, next: &ServerProvider) -> ServerProvider {
    let Some(previous) = previous else {
        return next.clone();
    };
    let saved_account = carry_saved_antigravity_account(previous, next);
    let mut merged = next.clone();
    if let Some((auth, status)) = saved_account {
        if status == ServerProviderState::Ready {
            merged.message = None;
        }
        merged.auth = auth;
        merged.status = status;
    }
    merged.models = merge_provider_models(next, &previous.models, &next.models);
    if next.workspace_snapshots.is_none() {
        merged.workspace_snapshots = previous.workspace_snapshots.clone();
    }
    if should_retain_missing_opencode_metadata(next) {
        if next.slash_commands.is_empty() {
            merged.slash_commands = previous.slash_commands.clone();
        }
        if next.skills.is_empty() {
            merged.skills = previous.skills.clone();
        }
    }
    merged
}

fn classify_compatibility(provider: &ServerProvider, manifest: &ModelManifestData) -> ServerProvider {
    apply_provider_compatibility(provider, manifest.compatibility.as_deref(), bundled_model_manifest().compatibility.as_deref())
}

fn correlated(instance: &ProviderInstance, snapshot: &ServerProvider) -> bool {
    if snapshot.instance_id != instance.instance_id {
        tracing::error!(source = %instance.instance_id, emitted = %snapshot.instance_id, "Provider snapshot instance mismatch");
        return false;
    }
    if snapshot.driver != instance.driver_kind {
        tracing::error!(instance = %instance.instance_id, source = %instance.driver_kind, emitted = %snapshot.driver, "Provider snapshot driver mismatch");
        return false;
    }
    true
}

#[derive(Default, Clone, Copy)]
struct UpsertOptions {
    no_publish: bool,
    no_persist: bool,
    replace: bool,
}

struct LiveSub {
    instance: Arc<ProviderInstance>,
    stop: CancellationToken,
}

struct Inner {
    instances: ProviderInstanceRegistry,
    manifest: ModelManifest,
    cache_dir: PathBuf,
    providers: Mutex<Vec<ServerProvider>>,
    workspace_refreshes: Mutex<Vec<(usize, String)>>,
    maintenance_states: Mutex<HashMap<ProviderInstanceId, ServerProviderUpdateState>>,
    live_subs: Mutex<Vec<(ProviderInstanceId, LiveSub)>>,
    sync_lock: tokio::sync::Mutex<()>,
    changes: PubSub<Vec<ServerProvider>>,
    compatibility_refresh_running: AtomicBool,
    shutdown: CancellationToken,
}

/// `ProviderRegistry`.
#[derive(Clone)]
pub struct ProviderRegistry {
    inner: Arc<Inner>,
}

impl ProviderRegistry {
    /// `ProviderRegistryLive`: hydrate from the cache, attach to every instance, follow the
    /// instance registry. Never runs a provider probe itself.
    pub async fn start(instances: ProviderInstanceRegistry, manifest: ModelManifest, cache_dir: PathBuf) -> Self {
        let boot_instances = instances.list_instances();
        let mut fallback = Vec::new();
        for instance in &boot_instances {
            let snapshot = instance.snapshot.get_snapshot().await;
            if correlated(instance, &snapshot) {
                fallback.push((instance.clone(), snapshot));
            }
        }
        let mut cached = Vec::new();
        for (instance, fallback_provider) in &fallback {
            let path = resolve_provider_status_cache_path(&cache_dir, instance.instance_id.as_str());
            let Some(cached_provider) = read_provider_status_cache(&path).await else {
                continue;
            };
            if !is_cached_provider_correlated(&cached_provider, fallback_provider) {
                tracing::warn!(path = %path.display(), instance_id = %instance.instance_id, "provider status cache identity mismatch, ignoring");
                continue;
            }
            cached.push(hydrate_cached_provider(&cached_provider, fallback_provider));
        }
        let initial_manifest = manifest.current().await;
        let providers: Vec<ServerProvider> = order_provider_snapshots(cached)
            .iter()
            .map(|provider| classify_compatibility(provider, &initial_manifest))
            .collect();
        let registry = Self {
            inner: Arc::new(Inner {
                instances: instances.clone(),
                manifest,
                cache_dir,
                providers: Mutex::new(providers),
                workspace_refreshes: Mutex::new(Vec::new()),
                maintenance_states: Mutex::new(HashMap::new()),
                live_subs: Mutex::new(Vec::new()),
                sync_lock: tokio::sync::Mutex::new(()),
                changes: PubSub::new(),
                compatibility_refresh_running: AtomicBool::new(false),
                shutdown: CancellationToken::new(),
            }),
        };
        registry
            .upsert_providers(
                fallback.into_iter().map(|(_, snapshot)| snapshot).collect(),
                UpsertOptions {
                    no_publish: true,
                    ..Default::default()
                },
            )
            .await;
        // Subscribe before the initial sync so no reconcile between the two is lost.
        let mut instance_changes = instances.subscribe_changes();
        registry.sync_live_sources().await;
        let watcher = registry.clone();
        let shutdown = registry.inner.shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    tick = instance_changes.recv() => match tick {
                        Some(()) => watcher.sync_live_sources().await,
                        None => break,
                    },
                }
            }
        });
        registry
    }

    /// Stop following instances (server shutdown).
    pub fn close(&self) {
        self.inner.shutdown.cancel();
        for (_, sub) in self.inner.live_subs.lock().unwrap().drain(..) {
            sub.stop.cancel();
        }
        self.inner.changes.shutdown();
    }

    /// `getProviders`.
    pub fn get_providers(&self) -> Vec<ServerProvider> {
        self.inner.providers.lock().unwrap().clone()
    }

    /// `streamChanges`: the full list after each effective change (eager subscription).
    pub fn subscribe_changes(&self) -> Subscription<Vec<ServerProvider>> {
        self.inner.changes.subscribe()
    }

    fn apply_update_state(&self, mut provider: ServerProvider) -> ServerProvider {
        provider.update_state = self.inner.maintenance_states.lock().unwrap().get(&provider.instance_id).cloned();
        provider
    }

    async fn persist(&self, provider: &ServerProvider) {
        let path = resolve_provider_status_cache_path(&self.inner.cache_dir, provider.instance_id.as_str());
        let mut machine = provider.clone();
        machine.workspace_snapshots = None;
        if let Err(error) = write_provider_status_cache(&path, &machine).await {
            tracing::error!(path = %path.display(), %error, "failed to write provider status cache");
        }
    }

    async fn upsert_providers(&self, next: Vec<ServerProvider>, options: UpsertOptions) -> Vec<ServerProvider> {
        let manifest = self.inner.manifest.current().await;
        let next: Vec<ServerProvider> = next.into_iter().map(|provider| self.apply_update_state(provider)).collect();
        let (changed, providers, to_persist) = {
            let mut current = self.inner.providers.lock().unwrap();
            let previous = current.clone();
            let mut merged: Vec<ServerProvider> = previous.clone();
            let mut updated: HashSet<ProviderInstanceId> = HashSet::new();
            for provider in next {
                updated.insert(provider.instance_id.clone());
                let index = merged.iter().position(|candidate| candidate.instance_id == provider.instance_id);
                let value = if options.replace {
                    provider
                } else {
                    merge_provider_snapshot(index.map(|index| &merged[index]), &provider)
                };
                match index {
                    Some(index) => merged[index] = value,
                    None => merged.push(value),
                }
            }
            let providers = order_provider_snapshots(merged.iter().map(|provider| classify_compatibility(provider, &manifest)).collect());
            let to_persist: Vec<ServerProvider> = providers.iter().filter(|provider| updated.contains(&provider.instance_id)).cloned().collect();
            let changed = previous != providers;
            *current = providers.clone();
            (changed, providers, to_persist)
        };
        if changed {
            if !options.no_persist {
                for provider in &to_persist {
                    self.persist(provider).await;
                }
            }
            if !options.no_publish {
                self.inner.changes.publish(providers.clone());
            }
        }
        providers
    }

    /// `syncProvider`: merge one snapshot, then (once at a time) refresh the manifest and
    /// reclassify the current list.
    async fn sync_provider(&self, provider: ServerProvider) -> Vec<ServerProvider> {
        let providers = self.upsert_providers(vec![provider], UpsertOptions::default()).await;
        if !self.inner.compatibility_refresh_running.swap(true, Ordering::SeqCst) {
            let registry = self.clone();
            tokio::spawn(async move {
                registry.inner.manifest.refresh().await;
                registry
                    .upsert_providers(
                        Vec::new(),
                        UpsertOptions {
                            no_persist: true,
                            ..Default::default()
                        },
                    )
                    .await;
                registry.inner.compatibility_refresh_running.store(false, Ordering::SeqCst);
            });
        }
        providers
    }

    fn publish_if_changed(&self, previous: &[ServerProvider], next: &[ServerProvider]) {
        if previous != next {
            self.inner.changes.publish(next.to_vec());
        }
    }

    /// `syncLiveSources`.
    async fn sync_live_sources(&self) {
        let _guard = self.inner.sync_lock.lock().await;
        let instances = self.inner.instances.list_instances();
        let unavailable = self.inner.instances.list_unavailable();
        let mut known: HashSet<ProviderInstanceId> = instances.iter().map(|instance| instance.instance_id.clone()).collect();
        known.extend(unavailable.iter().map(|provider| provider.instance_id.clone()));

        let previous_subs: Vec<(ProviderInstanceId, LiveSub)> = std::mem::take(&mut *self.inner.live_subs.lock().unwrap());
        let mut carried: Vec<(ProviderInstanceId, LiveSub)> = Vec::new();
        let mut previous_ids: HashSet<ProviderInstanceId> = HashSet::new();
        for (id, sub) in previous_subs {
            previous_ids.insert(id.clone());
            let same = instances
                .iter()
                .any(|instance| instance.instance_id == id && Arc::ptr_eq(instance, &sub.instance));
            if same {
                carried.push((id, sub));
            } else {
                sub.stop.cancel();
            }
        }
        let newly_added: Vec<Arc<ProviderInstance>> = instances
            .iter()
            .filter(|instance| !carried.iter().any(|(id, _)| id == &instance.instance_id))
            .cloned()
            .collect();

        let rebuilt: HashSet<ProviderInstanceId> = newly_added
            .iter()
            .map(|instance| instance.instance_id.clone())
            .filter(|id| previous_ids.contains(id))
            .collect();
        if !rebuilt.is_empty() {
            let (previous, next) = {
                let mut current = self.inner.providers.lock().unwrap();
                let previous = current.clone();
                for provider in current.iter_mut() {
                    if rebuilt.contains(&provider.instance_id) {
                        provider.workspace_snapshots = None;
                    }
                }
                (previous, current.clone())
            };
            self.publish_if_changed(&previous, &next);
        }

        // Subscribe to each new or rebuilt instance before reading its current snapshot.
        let mut new_subs = Vec::new();
        for instance in &newly_added {
            let stop = self.inner.shutdown.child_token();
            let mut stream = instance.snapshot.subscribe_changes();
            let registry = self.clone();
            let task_instance = instance.clone();
            let task_stop = stop.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = task_stop.cancelled() => break,
                        next = stream.next() => match next {
                            Some(snapshot) => {
                                if correlated(&task_instance, &snapshot) {
                                    registry.sync_provider(snapshot).await;
                                }
                            }
                            None => break,
                        },
                    }
                }
            });
            new_subs.push((
                instance.instance_id.clone(),
                LiveSub {
                    instance: instance.clone(),
                    stop,
                },
            ));
        }
        tokio::task::yield_now().await;
        for instance in &newly_added {
            let snapshot = instance.snapshot.get_snapshot().await;
            if correlated(instance, &snapshot) {
                self.sync_provider(snapshot).await;
            }
        }
        self.upsert_providers(
            unavailable,
            UpsertOptions {
                no_persist: true,
                replace: true,
                ..Default::default()
            },
        )
        .await;
        {
            let mut subs = self.inner.live_subs.lock().unwrap();
            subs.extend(carried);
            subs.extend(new_subs);
        }
        let (previous, next) = {
            let mut current = self.inner.providers.lock().unwrap();
            let previous = current.clone();
            let kept: Vec<ServerProvider> = previous.iter().filter(|provider| known.contains(&provider.instance_id)).cloned().collect();
            *current = order_provider_snapshots(kept);
            (previous, current.clone())
        };
        self.publish_if_changed(&previous, &next);
        self.inner.maintenance_states.lock().unwrap().retain(|id, _| known.contains(id));
    }

    fn live_instance(&self, instance_id: &ProviderInstanceId) -> Option<Arc<ProviderInstance>> {
        self.inner
            .live_subs
            .lock()
            .unwrap()
            .iter()
            .find(|(id, _)| id == instance_id)
            .map(|(_, sub)| sub.instance.clone())
    }

    async fn refresh_one(&self, instance: Arc<ProviderInstance>) -> Vec<ServerProvider> {
        let snapshot = instance.snapshot.refresh().await;
        if !correlated(&instance, &snapshot) {
            tracing::error!("provider registry refresh failed; preserving cached providers");
            return self.get_providers();
        }
        self.sync_provider(snapshot).await
    }

    /// `refresh(provider?)`: everything, or the default instance of `provider`.
    pub async fn refresh(&self, provider: Option<&ProviderDriverKind>) -> Vec<ServerProvider> {
        match provider {
            Some(kind) => self.refresh_instance(&ProviderInstanceId::from(kind.as_str())).await,
            None => {
                let instances: Vec<Arc<ProviderInstance>> = self.inner.live_subs.lock().unwrap().iter().map(|(_, sub)| sub.instance.clone()).collect();
                futures::future::join_all(instances.into_iter().map(|instance| self.refresh_one(instance))).await;
                self.get_providers()
            }
        }
    }

    /// `refreshInstance(instanceId)`: unknown ids answer the cached list.
    pub async fn refresh_instance(&self, instance_id: &ProviderInstanceId) -> Vec<ServerProvider> {
        match self.live_instance(instance_id) {
            Some(instance) => self.refresh_one(instance).await,
            None => self.get_providers(),
        }
    }

    /// `refreshWorkspaceSnapshot({instanceId, cwd})`: workspace-scoped skills and commands, at
    /// most one probe per (instance, cwd) at a time, 16 workspaces kept per provider.
    pub async fn refresh_workspace_snapshot(&self, instance_id: &ProviderInstanceId, cwd: &str) -> Vec<ServerProvider> {
        let providers = self.get_providers();
        let Some(provider) = providers.iter().find(|candidate| &candidate.instance_id == instance_id) else {
            return providers;
        };
        if !provider.enabled
            || provider
                .workspace_snapshots
                .as_ref()
                .is_some_and(|snapshots| snapshots.iter().any(|snapshot| snapshot.cwd == cwd))
        {
            return providers;
        }
        let Some(instance) = self.inner.instances.get_instance(instance_id) else {
            return providers;
        };
        let Some(snapshot_for_cwd) = instance.snapshot_for_cwd.clone() else {
            return providers;
        };
        let key = (Arc::as_ptr(&instance) as usize, cwd.to_owned());
        {
            let mut refreshes = self.inner.workspace_refreshes.lock().unwrap();
            if refreshes.contains(&key) {
                return self.get_providers();
            }
            refreshes.push(key.clone());
        }
        let result = snapshot_for_cwd(cwd.to_owned()).await;
        let providers = match result {
            Ok(scoped) if scoped.status != ServerProviderState::Error => {
                let still_current = self
                    .inner
                    .instances
                    .get_instance(instance_id)
                    .is_some_and(|current| Arc::ptr_eq(&current, &instance));
                if !still_current {
                    self.get_providers()
                } else {
                    let (previous, next) = {
                        let mut current = self.inner.providers.lock().unwrap();
                        let previous = current.clone();
                        let next: Vec<ServerProvider> = previous
                            .iter()
                            .map(|candidate| {
                                if &candidate.instance_id == instance_id
                                    && !candidate
                                        .workspace_snapshots
                                        .as_ref()
                                        .is_some_and(|snapshots| snapshots.iter().any(|snapshot| snapshot.cwd == cwd))
                                {
                                    upsert_provider_workspace_snapshot(candidate, cwd, &scoped)
                                } else {
                                    candidate.clone()
                                }
                            })
                            .collect();
                        *current = next.clone();
                        (previous, next)
                    };
                    self.publish_if_changed(&previous, &next);
                    next
                }
            }
            Ok(_) => self.get_providers(),
            Err(error) => {
                tracing::error!(%error, "provider registry refresh failed; preserving cached providers");
                self.get_providers()
            }
        };
        self.inner.workspace_refreshes.lock().unwrap().retain(|entry| entry != &key);
        providers
    }

    /// `getProviderMaintenanceCapabilitiesForInstance(instanceId, provider, {fresh})`.
    pub async fn get_provider_maintenance_capabilities_for_instance(
        &self,
        instance_id: &ProviderInstanceId,
        provider: &ProviderDriverKind,
        fresh: bool,
    ) -> ProviderMaintenanceCapabilities {
        match self.inner.instances.get_instance(instance_id) {
            Some(instance) if &instance.driver_kind == provider => instance.snapshot.resolve_maintenance(fresh).await,
            _ => ProviderMaintenanceCapabilities::manual_only(provider.clone(), None),
        }
    }

    /// `setProviderMaintenanceActionState({instanceId, action: "update", state})`: volatile,
    /// never persisted.
    pub async fn set_provider_maintenance_action_state(
        &self,
        instance_id: &ProviderInstanceId,
        state: Option<ServerProviderUpdateState>,
    ) -> Vec<ServerProvider> {
        {
            let mut states = self.inner.maintenance_states.lock().unwrap();
            match state {
                Some(state) if state.status != ServerProviderUpdateStatus::Idle => {
                    states.insert(instance_id.clone(), state);
                }
                _ => {
                    states.remove(instance_id);
                }
            }
        }
        let existing = self.get_providers();
        let Some(matching) = existing.iter().find(|candidate| &candidate.instance_id == instance_id).cloned() else {
            return existing;
        };
        self.upsert_providers(
            vec![matching],
            UpsertOptions {
                no_persist: true,
                ..Default::default()
            },
        )
        .await
    }
}

/// `ProviderUsageLimitsIngestionLive`: fold every `account.rate-limits.updated` runtime event
/// into its instance's published snapshot (which the registry already aggregates). Runs until
/// `stop` is cancelled or the event stream ends.
pub fn start_usage_limits_ingestion(
    service: &crate::ProviderServiceImpl,
    instances: ProviderInstanceRegistry,
    stop: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let mut events = service.subscribe_events();
    tokio::spawn(async move {
        loop {
            let event = tokio::select! {
                _ = stop.cancelled() => break,
                event = events.recv() => match event {
                    Some(event) => event,
                    None => break,
                },
            };
            let zc_contracts::ProviderRuntimeEvent::AccountRateLimitsUpdated(update) = event else {
                continue;
            };
            let Some(instance) = update.provider_instance_id.as_ref().and_then(|id| instances.get_instance(id)) else {
                continue;
            };
            instance.snapshot.apply_usage_limits(update.payload.limits, zc_core::now_iso()).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn provider(value: serde_json::Value) -> ServerProvider {
        let mut base = json!({
            "instanceId": "codex", "driver": "codex", "enabled": true, "installed": true, "version": "1.0.0",
            "status": "ready", "auth": {"status": "authenticated"}, "checkedAt": "2026-01-01T00:00:00.000Z",
            "models": [], "slashCommands": [], "skills": []
        });
        for (key, item) in value.as_object().unwrap() {
            base[key] = item.clone();
        }
        serde_json::from_value(base).unwrap()
    }

    fn model(slug: &str, custom: bool) -> serde_json::Value {
        json!({"slug": slug, "name": slug, "isCustom": custom, "capabilities": null})
    }

    fn slugs(provider: &ServerProvider) -> Vec<&str> {
        provider.models.iter().map(|model| model.slug.as_str()).collect()
    }

    #[test]
    fn stores_workspace_snapshots_without_changing_machine_metadata() {
        let machine = provider(json!({"driver": "claudeAgent", "instanceId": "claudeAgent", "skills": [{"name": "global", "path": "/g", "enabled": true}]}));
        let scoped = provider(
            json!({"driver": "claudeAgent", "instanceId": "claudeAgent", "checkedAt": "2026-02-01T00:00:00.000Z", "skills": [{"name": "local", "path": "/w/l", "enabled": true}]}),
        );
        let next = upsert_provider_workspace_snapshot(&machine, "/w", &scoped);
        assert_eq!(next.skills, machine.skills);
        let snapshots = next.workspace_snapshots.unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].cwd, "/w");
        assert_eq!(snapshots[0].skills[0].name, "local");
        let mut many = machine.clone();
        for index in 0..20 {
            many = upsert_provider_workspace_snapshot(&many, &format!("/w{index}"), &scoped);
        }
        let kept = many.workspace_snapshots.unwrap();
        assert_eq!(kept.len(), MAX_WORKSPACE_SNAPSHOTS_PER_PROVIDER);
        assert_eq!(kept.last().unwrap().cwd, "/w19");
    }

    #[test]
    fn keeps_models_a_failed_or_pending_refresh_did_not_report() {
        let previous = provider(json!({"driver": "claudeAgent", "instanceId": "claudeAgent", "models": [model("a", false), model("custom-old", true)]}));
        let next = provider(json!({"driver": "claudeAgent", "instanceId": "claudeAgent", "models": []}));
        assert_eq!(slugs(&merge_provider_snapshot(Some(&previous), &next)), vec!["a"]);
        let next_custom = provider(json!({"driver": "claudeAgent", "instanceId": "claudeAgent", "models": [model("b", false)]}));
        assert_eq!(slugs(&merge_provider_snapshot(Some(&previous), &next_custom)), vec!["b", "a"]);
    }

    #[test]
    fn drops_retired_codex_models_after_successful_discovery() {
        let previous = provider(json!({"models": [model("old", false)]}));
        let next = provider(json!({"models": [model("new", false)]}));
        assert_eq!(slugs(&merge_provider_snapshot(Some(&previous), &next)), vec!["new"]);
        // A failed probe of an installed provider keeps them.
        let failed = provider(json!({"status": "error", "models": [model("new", false)]}));
        assert_eq!(slugs(&merge_provider_snapshot(Some(&previous), &failed)), vec!["new", "old"]);
        // Sign-out clears them.
        let signed_out = provider(json!({"auth": {"status": "unauthenticated"}, "models": []}));
        assert!(slugs(&merge_provider_snapshot(Some(&previous), &signed_out)).is_empty());
    }

    #[test]
    fn opencode_metadata_survives_failed_refreshes_only() {
        let previous = provider(json!({"driver": "opencode", "instanceId": "opencode", "models": [model("x", false)], "slashCommands": [{"name": "init"}]}));
        let failed = provider(json!({"driver": "opencode", "instanceId": "opencode", "status": "error", "models": []}));
        let merged = merge_provider_snapshot(Some(&previous), &failed);
        assert_eq!(slugs(&merged), vec!["x"]);
        assert_eq!(merged.slash_commands.len(), 1);
        let ok = provider(json!({"driver": "opencode", "instanceId": "opencode", "models": [model("y", false)]}));
        let merged = merge_provider_snapshot(Some(&previous), &ok);
        assert_eq!(slugs(&merged), vec!["y"]);
        assert!(merged.slash_commands.is_empty());
    }

    #[test]
    fn carries_the_saved_antigravity_account_through_health_checks() {
        let previous = provider(
            json!({"driver": "antigravity", "instanceId": "antigravity", "auth": {"status": "authenticated", "type": "google", "email": "user@example.com"}}),
        );
        let health = provider(
            json!({"driver": "antigravity", "instanceId": "antigravity", "status": "warning", "auth": {"status": "unknown"}, "message": "Google account access is not checked yet."}),
        );
        let merged = merge_provider_snapshot(Some(&previous), &health);
        assert_eq!(merged.auth.status, ServerProviderAuthStatus::Authenticated);
        assert_eq!(merged.status, ServerProviderState::Ready);
        assert_eq!(merged.message, None);
        let other = provider(json!({"auth": {"status": "unknown"}, "status": "warning"}));
        assert_eq!(merge_provider_snapshot(Some(&previous), &other).auth.status, ServerProviderAuthStatus::Unknown);
    }

    #[test]
    fn fills_missing_capabilities_from_the_previous_snapshot() {
        let with_caps = json!({"slug": "m", "name": "m", "isCustom": false, "capabilities": {"optionDescriptors": [{"id": "effort", "label": "Effort", "type": "select", "options": []}]}});
        let previous = provider(json!({"driver": "claudeAgent", "instanceId": "claudeAgent", "models": [with_caps]}));
        let next = provider(json!({"driver": "claudeAgent", "instanceId": "claudeAgent", "models": [model("m", false)]}));
        let merged = merge_provider_snapshot(Some(&previous), &next);
        assert!(has_model_capabilities(&merged.models[0]));
    }
}
