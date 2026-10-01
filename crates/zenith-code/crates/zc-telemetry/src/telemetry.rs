//! `resourceTelemetry/ResourceTelemetry.ts`: the server's resource telemetry service.
//!
//! - `latest`: the last merged snapshot (native + desktop + attribution + health);
//! - `subscribe`: the latest snapshot and its changes, atomically; while at least one
//!   subscription lives, native snapshots stream in (1 Hz) and the desktop gets diagnostics
//!   demand;
//! - `refresh`: a fresh native sample now; `readHistory`: the monitor's retained snapshots
//!   replayed into buckets; `retry`: restart a monitor that gave up.

use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};

use futures::{Stream, StreamExt};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::BroadcastStream;
use zc_contracts::{
    DateTimeUtc, DesktopHostTelemetrySnapshot, DesktopHostTelemetrySnapshotPower, EOption, HostPowerSnapshot, ResourceMonitorExternalProcess,
    ResourceMonitorSnapshotEvent, ResourceTelemetryHealth, ResourceTelemetryProcessIdentity, ResourceTelemetryRetryResult, ResourceTelemetrySnapshot,
    ResourceTelemetrySourceHealth,
};

use crate::attribution::ResourceAttribution;
use crate::desktop::{DesktopHealth, DesktopTelemetry};
use crate::history::{build_history, normalize_history_input, BuildHistoryInput, HistoryWithLegacyBuckets};
use crate::model::{merge_processes, MergeProcessesInput, ProcessState, TelemetryCounters};
use crate::native::{unknown_power, NativeError, NativeHealth, NativeTelemetry, NativeTelemetrySnapshot};

/// `ResourceTelemetryRefreshFailed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshFailed {
    pub operation: &'static str,
    pub cause: NativeError,
}

impl std::fmt::Display for RefreshFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Resource telemetry operation '{}' failed.", self.operation)
    }
}

impl std::error::Error for RefreshFailed {}

struct TelemetryState {
    native_snapshot: Option<ResourceMonitorSnapshotEvent>,
    desktop_snapshot: Option<DesktopHostTelemetrySnapshot>,
    previous: std::collections::HashMap<String, ProcessState>,
    counters: TelemetryCounters,
    latest: ResourceTelemetrySnapshot,
    last_native_sequence: i64,
    last_native_generation: i64,
}

#[derive(Default)]
struct LiveState {
    retain_count: usize,
    tasks: Vec<JoinHandle<()>>,
}

/// The service; build it with [`ResourceTelemetry::start`].
pub struct ResourceTelemetry {
    native: Arc<dyn NativeTelemetry>,
    desktop: Arc<dyn DesktopTelemetry>,
    attribution: ResourceAttribution,
    server_pid: i64,
    state: Mutex<TelemetryState>,
    changes: broadcast::Sender<ResourceTelemetrySnapshot>,
    live: Mutex<LiveState>,
    watchers: Mutex<Vec<JoinHandle<()>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

fn desktop_power(power: &DesktopHostTelemetrySnapshotPower) -> HostPowerSnapshot {
    HostPowerSnapshot {
        source: power.source,
        idle: power.idle,
        idle_seconds: power.idle_seconds,
        locked: power.locked,
        suspended: power.suspended,
        on_battery: power.on_battery,
        low_power_mode: power.low_power_mode,
        thermal_state: power.thermal_state,
        stale: power.stale,
        updated_at: power.updated_at,
    }
}

fn source_health(
    status: zc_contracts::ResourceTelemetrySourceStatus,
    last_sample_at: Option<DateTimeUtc>,
    last_error: Option<String>,
) -> ResourceTelemetrySourceHealth {
    ResourceTelemetrySourceHealth {
        status,
        last_sample_at: EOption(last_sample_at),
        last_error: EOption(last_error),
    }
}

/// `buildHealth`.
pub fn build_health(native: &NativeHealth, desktop: &DesktopHealth, native_snapshot: Option<&ResourceMonitorSnapshotEvent>) -> ResourceTelemetryHealth {
    ResourceTelemetryHealth {
        native: source_health(native.status, native.last_sample_at, native.last_error.clone()),
        desktop: source_health(desktop.status, desktop.last_sample_at, desktop.last_error.clone()),
        sidecar_version: EOption(native.hello.as_ref().map(|h| h.sidecar_version.clone())),
        sidecar_pid: EOption(native.hello.as_ref().map(|h| h.sidecar_pid)),
        restart_count: native.restart_count,
        collection_duration_micros: native_snapshot.map_or(0, |s| s.collection_duration_micros),
        scanned_process_count: native_snapshot.map_or(0, |s| s.scanned_process_count),
        retained_process_count: native_snapshot.map_or(0, |s| s.retained_process_count),
        inaccessible_process_count: native_snapshot.map_or(0, |s| s.inaccessible_process_count),
    }
}

fn electron_root(desktop: &DesktopHostTelemetrySnapshot) -> ResourceMonitorExternalProcess {
    ResourceMonitorExternalProcess {
        pid: desktop.electron_pid,
        start_time_ms: desktop
            .electron_processes
            .iter()
            .find(|p| p.pid == desktop.electron_pid)
            .map(|p| p.creation_time_ms),
    }
}

/// A [`ResourceTelemetry::subscribe`] change stream; it keeps live collection on while it exists.
pub struct LiveChanges {
    stream: BroadcastStream<ResourceTelemetrySnapshot>,
    owner: Weak<ResourceTelemetry>,
}

impl Stream for LiveChanges {
    type Item = ResourceTelemetrySnapshot;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match Pin::new(&mut self.stream).poll_next(cx) {
                Poll::Ready(Some(Ok(item))) => return Poll::Ready(Some(item)),
                // A sliding buffer of 8: lagging subscribers lose the oldest snapshots.
                Poll::Ready(Some(Err(_))) => continue,
                other => return other.map(|_| None),
            }
        }
    }
}

impl Drop for LiveChanges {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade() {
            owner.release_live();
        }
    }
}

impl ResourceTelemetry {
    /// Builds the service and starts its health watchers (needs a Tokio runtime). With a
    /// desktop snapshot already there, the native source learns the Electron root in the
    /// background.
    pub fn start(native: Arc<dyn NativeTelemetry>, desktop: Arc<dyn DesktopTelemetry>, attribution: ResourceAttribution, server_pid: i64) -> Arc<Self> {
        let initial_read_at = DateTimeUtc::now();
        let (native_health, native_changes) = native.subscribe_health();
        let initial_desktop = desktop.latest();
        if let Some(desktop_snapshot) = &initial_desktop {
            let native = native.clone();
            let root = electron_root(desktop_snapshot);
            let power = desktop_power(&desktop_snapshot.power);
            tokio::spawn(async move {
                let _ = native.set_external_processes(vec![root]).await;
                let _ = native.set_host_power_state(power).await;
            });
        }
        let desktop_health = desktop.health();
        let merged = merge_processes(MergeProcessesInput {
            server_pid,
            sidecar_pid: native_health.hello.as_ref().map(|h| h.sidecar_pid),
            fallback_sampled_at_ms: initial_read_at.as_millis(),
            native_snapshot: None,
            desktop_snapshot: initial_desktop.as_ref(),
            electron_root_pids: None,
            electron_root_start_times: None,
            previous: &Default::default(),
            counters: TelemetryCounters::default(),
            update_previous: false,
        });
        let initial = ResourceTelemetrySnapshot {
            read_at: initial_read_at,
            sample_interval_ms: native_health.sample_interval_ms,
            processes: merged.processes,
            groups: merged.groups,
            power: initial_desktop
                .as_ref()
                .map(|d| desktop_power(&d.power))
                .unwrap_or_else(|| unknown_power(initial_read_at)),
            speed_limit_percent: EOption(initial_desktop.as_ref().and_then(|d| d.speed_limit_percent)),
            attribution: attribution.snapshot(),
            health: build_health(&native_health, &desktop_health, None),
        };
        let telemetry = Arc::new(Self {
            native,
            desktop,
            attribution,
            server_pid,
            state: Mutex::new(TelemetryState {
                native_snapshot: None,
                desktop_snapshot: initial_desktop,
                previous: Default::default(),
                counters: TelemetryCounters::default(),
                latest: initial,
                last_native_sequence: 0,
                last_native_generation: native_health.restart_count,
            }),
            changes: broadcast::channel(8).0,
            live: Mutex::new(LiveState::default()),
            watchers: Mutex::new(Vec::new()),
        });
        let weak = Arc::downgrade(&telemetry);
        let watcher = tokio::spawn(async move {
            let mut changes = native_changes;
            while changes.next().await.is_some() {
                match weak.upgrade() {
                    Some(telemetry) => telemetry.refresh_health(),
                    None => return,
                }
            }
        });
        lock(&telemetry.watchers).push(watcher);
        telemetry
    }

    /// Stops the watchers and any live collection.
    pub fn shutdown(&self) {
        for task in lock(&self.watchers).drain(..) {
            task.abort();
        }
        let mut live = lock(&self.live);
        for task in live.tasks.drain(..) {
            task.abort();
        }
    }

    pub fn attribution(&self) -> &ResourceAttribution {
        &self.attribution
    }

    pub fn server_pid(&self) -> i64 {
        self.server_pid
    }

    /// The last merged snapshot.
    pub fn latest(&self) -> ResourceTelemetrySnapshot {
        lock(&self.state).latest.clone()
    }

    fn is_live(&self) -> bool {
        lock(&self.live).retain_count > 0
    }

    fn refresh_health(&self) {
        let mut state = lock(&self.state);
        let health = build_health(&self.native.health(), &self.desktop.health(), state.native_snapshot.as_ref());
        state.latest.health = health;
        let snapshot = state.latest.clone();
        if self.is_live() {
            let _ = self.changes.send(snapshot);
        }
    }

    /// `rebuild`: merges the new native and/or desktop data into the next snapshot.
    fn rebuild(
        &self,
        native: Option<NativeTelemetrySnapshot>,
        desktop: Option<DesktopHostTelemetrySnapshot>,
        update_previous: bool,
        publish_when_live: bool,
    ) -> ResourceTelemetrySnapshot {
        let mut state = lock(&self.state);
        let native_health = self.native.health();
        if let Some(incoming) = &native {
            if incoming.generation < native_health.restart_count
                || incoming.generation < state.last_native_generation
                || (incoming.generation == state.last_native_generation && incoming.snapshot.sequence <= state.last_native_sequence)
            {
                return state.latest.clone();
            }
        }
        let (incoming_sequence, incoming_generation) = native
            .as_ref()
            .map(|n| (n.snapshot.sequence, n.generation))
            .unwrap_or((state.last_native_sequence, state.last_native_generation));
        let native_snapshot = native.map(|n| n.snapshot).or_else(|| state.native_snapshot.clone());
        let desktop_snapshot = desktop.or_else(|| state.desktop_snapshot.clone());
        let desktop_health = self.desktop.health();
        let attribution = self.attribution.snapshot();

        let mut roots: Vec<(i64, Option<i64>)> = native_snapshot
            .as_ref()
            .and_then(|s| s.external_processes.as_ref())
            .map(|e| e.iter().map(|p| (p.pid, p.start_time_ms)).collect())
            .unwrap_or_default();
        let mut root_pids: Vec<i64> = roots.iter().map(|(pid, _)| *pid).collect();
        if let Some(desktop) = &desktop_snapshot {
            root_pids.push(desktop.electron_pid);
        }
        let root_start_times: std::collections::HashMap<i64, i64> = roots.drain(..).filter_map(|(pid, start)| start.map(|s| (pid, s))).collect();
        let merged = merge_processes(MergeProcessesInput {
            server_pid: self.server_pid,
            sidecar_pid: native_health.hello.as_ref().map(|h| h.sidecar_pid),
            fallback_sampled_at_ms: state.latest.read_at.as_millis(),
            native_snapshot: native_snapshot.as_ref(),
            desktop_snapshot: desktop_snapshot.as_ref(),
            electron_root_pids: Some(&root_pids),
            electron_root_start_times: Some(&root_start_times),
            previous: &state.previous,
            counters: state.counters,
            update_previous,
        });
        let read_at = DateTimeUtc::from_millis(merged.sampled_at_ms).unwrap_or_else(|_| DateTimeUtc::now());
        let snapshot = ResourceTelemetrySnapshot {
            read_at,
            sample_interval_ms: native_health.sample_interval_ms,
            processes: merged.processes,
            groups: merged.groups,
            power: desktop_snapshot
                .as_ref()
                .map(|d| desktop_power(&d.power))
                .unwrap_or_else(|| unknown_power(read_at)),
            speed_limit_percent: EOption(desktop_snapshot.as_ref().and_then(|d| d.speed_limit_percent)),
            attribution,
            health: build_health(&native_health, &desktop_health, native_snapshot.as_ref()),
        };
        state.native_snapshot = native_snapshot;
        state.desktop_snapshot = desktop_snapshot;
        state.previous = merged.previous;
        state.counters = merged.counters;
        state.latest = snapshot.clone();
        state.last_native_sequence = incoming_sequence;
        state.last_native_generation = incoming_generation;
        if !publish_when_live || self.is_live() {
            let _ = self.changes.send(snapshot.clone());
        }
        snapshot
    }

    fn ingest_native(&self, snapshot: NativeTelemetrySnapshot) -> ResourceTelemetrySnapshot {
        self.rebuild(Some(snapshot), None, true, false)
    }

    /// A desktop telemetry update (the Electron shell's metrics and power state).
    pub async fn ingest_desktop(&self, snapshot: DesktopHostTelemetrySnapshot) -> ResourceTelemetrySnapshot {
        let _ = self.native.set_external_processes(vec![electron_root(&snapshot)]).await;
        let _ = self.native.set_host_power_state(desktop_power(&snapshot.power)).await;
        self.rebuild(None, Some(snapshot), false, true)
    }

    fn acquire_live(self: &Arc<Self>) {
        let mut live = lock(&self.live);
        live.retain_count += 1;
        if live.retain_count > 1 {
            return;
        }
        self.desktop.set_diagnostics_demand(true);
        let weak = Arc::downgrade(self);
        let mut snapshots = self.native.snapshots();
        live.tasks.push(tokio::spawn(async move {
            while let Some(snapshot) = snapshots.next().await {
                match weak.upgrade() {
                    Some(telemetry) => {
                        telemetry.ingest_native(snapshot);
                    }
                    None => return,
                }
            }
            tracing::warn!("native resource telemetry stream stopped");
        }));
        let weak = Arc::downgrade(self);
        let native = self.native.clone();
        live.tasks.push(tokio::spawn(async move {
            if let Ok(snapshot) = native.sample_now().await {
                if let Some(telemetry) = weak.upgrade() {
                    telemetry.ingest_native(snapshot);
                }
            }
        }));
    }

    fn release_live(&self) {
        let mut live = lock(&self.live);
        if live.retain_count <= 1 {
            live.retain_count = 0;
            for task in live.tasks.drain(..) {
                task.abort();
            }
            drop(live);
            self.desktop.set_diagnostics_demand(false);
            return;
        }
        live.retain_count -= 1;
    }

    /// The latest snapshot and every later one, atomically; live collection runs until the
    /// change stream is dropped.
    pub fn subscribe(self: &Arc<Self>) -> (ResourceTelemetrySnapshot, LiveChanges) {
        self.acquire_live();
        let state = lock(&self.state);
        let receiver = self.changes.subscribe();
        let latest = state.latest.clone();
        drop(state);
        (
            latest,
            LiveChanges {
                stream: BroadcastStream::new(receiver),
                owner: Arc::downgrade(self),
            },
        )
    }

    /// `refresh`: a native sample now, merged.
    pub async fn refresh(&self) -> Result<ResourceTelemetrySnapshot, RefreshFailed> {
        match self.native.sample_now().await {
            Ok(snapshot) => Ok(self.ingest_native(snapshot)),
            Err(cause) => Err(RefreshFailed { operation: "refresh", cause }),
        }
    }

    /// `refresh`, or the latest snapshot when sampling fails.
    pub async fn refreshed_or_latest(&self) -> ResourceTelemetrySnapshot {
        match self.refresh().await {
            Ok(snapshot) => snapshot,
            Err(_) => self.latest(),
        }
    }

    /// `validateProcessIdentity`.
    pub async fn validate_process_identity(&self, identity: &ResourceTelemetryProcessIdentity) -> Result<bool, RefreshFailed> {
        match self.native.sample_now().await {
            Ok(sample) => Ok(sample
                .snapshot
                .processes
                .iter()
                .any(|p| p.pid == identity.pid && p.start_time_ms == identity.start_time_ms)),
            Err(cause) => Err(RefreshFailed {
                operation: "validateProcessIdentity",
                cause,
            }),
        }
    }

    /// `readHistory` (with the backend-only buckets).
    pub async fn read_history(&self, window_ms: i64, bucket_ms: i64) -> HistoryWithLegacyBuckets {
        let read_at = DateTimeUtc::now();
        let (window_ms, bucket_ms) = normalize_history_input(window_ms, bucket_ms);
        let snapshots = match self.native.read_history(window_ms).await {
            Ok(snapshots) => snapshots,
            Err(error) => {
                tracing::warn!(cause = %error, "Failed to read native resource telemetry history");
                Vec::new()
            }
        };
        let native_health = self.native.health();
        let desktop_health = self.desktop.health();
        let (desktop_snapshot, native_snapshot) = {
            let state = lock(&self.state);
            (state.desktop_snapshot.clone(), state.native_snapshot.clone())
        };
        build_history(BuildHistoryInput {
            read_at_ms: read_at.as_millis(),
            window_ms,
            bucket_ms,
            sample_interval_ms: native_health.sample_interval_ms,
            server_pid: self.server_pid,
            sidecar_pid: native_health.hello.as_ref().map(|h| h.sidecar_pid),
            desktop_snapshot: desktop_snapshot.as_ref(),
            snapshots: &snapshots,
            health: build_health(&native_health, &desktop_health, native_snapshot.as_ref()),
        })
    }

    /// `retry`: asks a monitor that gave up to start again.
    pub async fn retry(&self) -> ResourceTelemetryRetryResult {
        let accepted = self.native.retry().await;
        ResourceTelemetryRetryResult {
            accepted,
            snapshot: self.latest(),
        }
    }
}
