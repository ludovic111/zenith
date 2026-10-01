//! `resourceTelemetry/NativeTelemetryClient.ts`: drives the [`crate::monitor`] and supervises it.
//!
//! - health: `starting` → `healthy` once configured; `degraded` after a failure (restarted with
//!   a 0.5 s → 10 s backoff), `unavailable` after 5 failures within a minute (until a retry);
//! - collection control: 1 sample/s while live subscribers stream, every 5 s otherwise, slower
//!   on battery or under host constraints ([`resolve_native_sample_interval_ms`]);
//! - `sampleNow`, `readHistory`, `processTable` requests with timeouts; buffered live
//!   snapshots tagged with the monitor's generation (its restart count when started).

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::{Stream, StreamExt};
use tokio::sync::{broadcast, mpsc, oneshot, Notify};
use tokio_stream::wrappers::BroadcastStream;
use zc_contracts::{
    BackgroundBooleanState, DateTimeUtc, HostPowerSnapshot, HostPowerSource, HostPowerThermalState, ResourceMonitorExternalProcess,
    ResourceMonitorProcessTableEntry, ResourceMonitorSnapshotEvent, ResourceTelemetrySourceStatus,
};

use crate::monitor::{self, MonitorCommand, MonitorEvent, MonitorHandle};

const SAMPLE_INTERVAL_MS: i64 = 1_000;
const UNKNOWN_BACKGROUND_SAMPLE_INTERVAL_MS: i64 = 5_000;
const BATTERY_SAMPLE_INTERVAL_MS: i64 = 5_000;
const CONSTRAINED_SAMPLE_INTERVAL_MS: i64 = 15_000;
const SAMPLE_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const PROCESS_TABLE_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const HISTORY_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const INITIAL_RESTART_DELAY: Duration = Duration::from_millis(500);
const MAX_RESTART_DELAY: Duration = Duration::from_secs(10);
const FAILURE_WINDOW_MS: i64 = 60_000;
const MAX_FAILURES_PER_WINDOW: usize = 5;

/// What the monitor said hello with.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorHello {
    pub sidecar_version: String,
    pub sidecar_pid: i64,
}

/// `NativeTelemetryClientHealth`.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeHealth {
    pub status: ResourceTelemetrySourceStatus,
    pub hello: Option<MonitorHello>,
    pub last_sample_at: Option<DateTimeUtc>,
    pub last_error: Option<String>,
    pub restart_count: i64,
    pub sample_interval_ms: i64,
}

/// A snapshot and the monitor generation that took it.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeTelemetrySnapshot {
    pub generation: i64,
    pub snapshot: ResourceMonitorSnapshotEvent,
}

/// `NativeTelemetryClientError`; its text is the TS error's `message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeError {
    Unavailable(String),
    RequestTimedOut { operation: &'static str, timeout_ms: u128 },
    CommandFailed(String),
    Exited,
    SpawnFailed(String),
}

impl std::fmt::Display for NativeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(reason) => write!(f, "Resource monitor is unavailable: {reason}"),
            Self::RequestTimedOut { operation, timeout_ms } => write!(f, "Resource monitor '{operation}' request timed out after {timeout_ms}ms."),
            Self::CommandFailed(operation) => write!(f, "Resource monitor command '{operation}' failed."),
            Self::Exited => f.write_str("Resource monitor event stream closed unexpectedly."),
            Self::SpawnFailed(error) => write!(f, "Failed to start resource monitor '{error}'."),
        }
    }
}

impl std::error::Error for NativeError {}

/// The native telemetry source [`crate::ResourceTelemetry`] reads (a trait so tests can fake it,
/// like the TS `layerTest`).
#[async_trait]
pub trait NativeTelemetry: Send + Sync + 'static {
    /// Live snapshots; holding the stream counts as a live subscriber (faster sampling).
    fn snapshots(&self) -> BoxStream<'static, NativeTelemetrySnapshot>;
    async fn read_history(&self, window_ms: i64) -> Result<Vec<ResourceMonitorSnapshotEvent>, NativeError>;
    async fn set_external_processes(&self, processes: Vec<ResourceMonitorExternalProcess>) -> Result<(), NativeError>;
    async fn set_host_power_state(&self, snapshot: HostPowerSnapshot) -> Result<(), NativeError>;
    async fn sample_now(&self) -> Result<NativeTelemetrySnapshot, NativeError>;
    async fn process_table(&self) -> Result<Vec<ResourceMonitorProcessTableEntry>, NativeError>;
    async fn retry(&self) -> bool;
    fn health(&self) -> NativeHealth;
    /// The current health and its later changes (subscribed before reading the current one).
    fn subscribe_health(&self) -> (NativeHealth, BoxStream<'static, NativeHealth>);
}

pub fn unknown_power(updated_at: DateTimeUtc) -> HostPowerSnapshot {
    HostPowerSnapshot {
        source: HostPowerSource::Unknown,
        idle: BackgroundBooleanState::Unknown,
        idle_seconds: None,
        locked: BackgroundBooleanState::Unknown,
        suspended: false,
        on_battery: BackgroundBooleanState::Unknown,
        low_power_mode: BackgroundBooleanState::Unknown,
        thermal_state: HostPowerThermalState::Unknown,
        stale: true,
        updated_at,
    }
}

/// `resolveNativeSampleIntervalMs`.
pub fn resolve_native_sample_interval_ms(snapshot: &HostPowerSnapshot, live_subscriber_count: usize) -> i64 {
    let live = live_subscriber_count > 0;
    if snapshot.stale || snapshot.source == HostPowerSource::Unknown {
        return if live { SAMPLE_INTERVAL_MS } else { UNKNOWN_BACKGROUND_SAMPLE_INTERVAL_MS };
    }
    let thermally_constrained = matches!(snapshot.thermal_state, HostPowerThermalState::Serious | HostPowerThermalState::Critical);
    if snapshot.suspended || snapshot.locked == BackgroundBooleanState::True || snapshot.low_power_mode == BackgroundBooleanState::True || thermally_constrained
    {
        return CONSTRAINED_SAMPLE_INTERVAL_MS;
    }
    if snapshot.on_battery == BackgroundBooleanState::True {
        return BATTERY_SAMPLE_INTERVAL_MS;
    }
    if live {
        SAMPLE_INTERVAL_MS
    } else {
        UNKNOWN_BACKGROUND_SAMPLE_INTERVAL_MS
    }
}

/// `canRequestNativeTelemetryRetry`: only while the supervisor waits without a monitor.
pub fn can_request_retry(status: ResourceTelemetrySourceStatus, has_handle: bool) -> bool {
    status != ResourceTelemetrySourceStatus::Healthy && status != ResourceTelemetrySourceStatus::Starting && !has_handle
}

/// `canCommandNativeTelemetrySidecar`: a running monitor, healthy or degraded.
pub fn can_command(status: ResourceTelemetrySourceStatus, has_handle: bool) -> bool {
    has_handle && matches!(status, ResourceTelemetrySourceStatus::Healthy | ResourceTelemetrySourceStatus::Degraded)
}

/// `retainRecentNativeTelemetryFailures`.
pub fn retain_recent_failures(failures: &[i64], now: i64) -> Vec<i64> {
    failures.iter().copied().filter(|failed_at| now - failed_at <= FAILURE_WINDOW_MS).collect()
}

fn restart_delay(attempt: u32) -> Duration {
    INITIAL_RESTART_DELAY.saturating_mul(2u32.saturating_pow(attempt)).min(MAX_RESTART_DELAY)
}

#[derive(Debug, Clone)]
struct CollectionControl {
    host_power: HostPowerSnapshot,
    live_subscriber_count: usize,
    sample_interval_ms: i64,
}

struct ClientState {
    status: ResourceTelemetrySourceStatus,
    handle: Option<Arc<MonitorHandle>>,
    hello: Option<MonitorHello>,
    last_sample_at: Option<DateTimeUtc>,
    last_error: Option<String>,
    restart_count: i64,
}

type Pending<T> = Mutex<HashMap<String, oneshot::Sender<Result<T, NativeError>>>>;

struct Inner {
    root_pid: u32,
    state: Mutex<ClientState>,
    /// Desired and applied collection control (the monitor gets the difference).
    control: Mutex<(CollectionControl, CollectionControl)>,
    external: Mutex<Vec<ResourceMonitorExternalProcess>>,
    pending_samples: Pending<NativeTelemetrySnapshot>,
    pending_tables: Pending<Vec<ResourceMonitorProcessTableEntry>>,
    pending_histories: Pending<Vec<ResourceMonitorSnapshotEvent>>,
    snapshots: broadcast::Sender<NativeTelemetrySnapshot>,
    health_changes: broadcast::Sender<NativeHealth>,
    retry: Notify,
    supervisor: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

/// The real native telemetry: an in-process [`crate::monitor`] under a supervisor.
#[derive(Clone)]
pub struct NativeTelemetryClient {
    inner: Arc<Inner>,
}

impl NativeTelemetryClient {
    /// Starts the supervisor (needs a Tokio runtime). `root_pid` is the server's pid.
    pub fn start(root_pid: u32) -> Self {
        let now = DateTimeUtc::now();
        let control = CollectionControl {
            host_power: unknown_power(now),
            live_subscriber_count: 0,
            sample_interval_ms: UNKNOWN_BACKGROUND_SAMPLE_INTERVAL_MS,
        };
        let client = Self {
            inner: Arc::new(Inner {
                root_pid,
                state: Mutex::new(ClientState {
                    status: ResourceTelemetrySourceStatus::Starting,
                    handle: None,
                    hello: None,
                    last_sample_at: None,
                    last_error: None,
                    restart_count: 0,
                }),
                control: Mutex::new((control.clone(), control)),
                external: Mutex::new(Vec::new()),
                pending_samples: Mutex::new(HashMap::new()),
                pending_tables: Mutex::new(HashMap::new()),
                pending_histories: Mutex::new(HashMap::new()),
                snapshots: broadcast::channel(8).0,
                health_changes: broadcast::channel(4).0,
                retry: Notify::new(),
                supervisor: Mutex::new(None),
            }),
        };
        let task = tokio::spawn(supervise(client.inner.clone()));
        *lock(&client.inner.supervisor) = Some(task);
        client
    }

    /// Stops the supervisor and the monitor thread.
    pub fn shutdown(&self) {
        if let Some(task) = lock(&self.inner.supervisor).take() {
            task.abort();
        }
        let handle = lock(&self.inner.state).handle.take();
        if let Some(handle) = handle {
            let _ = handle.send(MonitorCommand::Shutdown);
        }
        let mut state = lock(&self.inner.state);
        state.status = ResourceTelemetrySourceStatus::Stopped;
        state.hello = None;
    }
}

impl Inner {
    fn health(&self) -> NativeHealth {
        let interval = lock(&self.control).0.sample_interval_ms;
        let state = lock(&self.state);
        NativeHealth {
            status: state.status,
            hello: state.hello.clone(),
            last_sample_at: state.last_sample_at,
            last_error: state.last_error.clone(),
            restart_count: state.restart_count,
            sample_interval_ms: interval,
        }
    }

    fn publish_health(&self) {
        let _ = self.health_changes.send(self.health());
    }

    fn commandable_handle(&self) -> Result<Arc<MonitorHandle>, NativeError> {
        let state = lock(&self.state);
        match &state.handle {
            Some(handle) if can_command(state.status, true) => Ok(handle.clone()),
            _ => Err(NativeError::Unavailable(
                state.last_error.clone().unwrap_or_else(|| "sidecar is not running".to_owned()),
            )),
        }
    }

    fn fail_pending(&self, error: &NativeError) {
        for (_, sender) in lock(&self.pending_samples).drain() {
            let _ = sender.send(Err(error.clone()));
        }
        for (_, sender) in lock(&self.pending_tables).drain() {
            let _ = sender.send(Err(error.clone()));
        }
        for (_, sender) in lock(&self.pending_histories).drain() {
            let _ = sender.send(Err(error.clone()));
        }
    }

    /// Applies a control change: the monitor gets the new interval and streaming state.
    fn update_control(&self, update: impl FnOnce(&mut CollectionControl)) {
        let mut control = lock(&self.control);
        update(&mut control.0);
        let (desired, applied) = (control.0.clone(), control.1.clone());
        let handle = {
            let state = lock(&self.state);
            state.handle.clone().filter(|_| can_command(state.status, true))
        };
        if let Some(handle) = handle {
            let mut ok = true;
            if applied.sample_interval_ms != desired.sample_interval_ms {
                ok &= handle.send(MonitorCommand::SetSampleInterval(desired.sample_interval_ms as u64)).is_ok();
            }
            let (was, is) = (applied.live_subscriber_count > 0, desired.live_subscriber_count > 0);
            if was != is {
                ok &= handle.send(MonitorCommand::SetStreaming(is)).is_ok();
            }
            if ok {
                control.1 = desired;
            }
        }
        drop(control);
        self.publish_health();
    }

    fn change_live_subscribers(&self, delta: isize) {
        self.update_control(|control| {
            control.live_subscriber_count = control.live_subscriber_count.saturating_add_signed(delta);
            control.sample_interval_ms = resolve_native_sample_interval_ms(&control.host_power, control.live_subscriber_count);
        });
    }

    fn process_event(&self, event: MonitorEvent, generation: i64) -> Result<(), NativeError> {
        match event {
            MonitorEvent::Hello {
                sidecar_version, sidecar_pid, ..
            } => {
                let mut state = lock(&self.state);
                state.status = ResourceTelemetrySourceStatus::Starting;
                state.hello = Some(MonitorHello {
                    sidecar_version: sidecar_version.to_owned(),
                    sidecar_pid: i64::from(sidecar_pid),
                });
                state.last_error = None;
                drop(state);
                self.publish_health();
            }
            MonitorEvent::Snapshot(snapshot) => {
                let changed = {
                    let mut state = lock(&self.state);
                    let changed = state.status != ResourceTelemetrySourceStatus::Healthy || state.last_error.is_some();
                    state.status = ResourceTelemetrySourceStatus::Healthy;
                    state.last_sample_at = DateTimeUtc::from_millis(snapshot.sampled_at_unix_ms).ok();
                    state.last_error = None;
                    changed
                };
                if changed {
                    self.publish_health();
                }
                let request_id = snapshot.request_id.clone();
                let native = NativeTelemetrySnapshot { generation, snapshot };
                let _ = self.snapshots.send(native.clone());
                if let Some(sender) = request_id.and_then(|id| lock(&self.pending_samples).remove(&id)) {
                    let _ = sender.send(Ok(native));
                }
            }
            MonitorEvent::ProcessTable { request_id, processes } => {
                if let Some(sender) = lock(&self.pending_tables).remove(&request_id) {
                    let _ = sender.send(Ok(processes));
                }
            }
            MonitorEvent::HistoryChunk { request_id, snapshots, .. } => {
                let changed = {
                    let mut state = lock(&self.state);
                    let changed = state.status != ResourceTelemetrySourceStatus::Healthy || state.last_error.is_some();
                    state.status = ResourceTelemetrySourceStatus::Healthy;
                    if let Some(latest) = snapshots.last() {
                        state.last_sample_at = DateTimeUtc::from_millis(latest.sampled_at_unix_ms).ok();
                    }
                    state.last_error = None;
                    changed
                };
                if changed {
                    self.publish_health();
                }
                if let Some(sender) = lock(&self.pending_histories).remove(&request_id) {
                    let _ = sender.send(Ok(snapshots));
                }
            }
            MonitorEvent::Error {
                code, message, recoverable, ..
            } => {
                {
                    let mut state = lock(&self.state);
                    state.status = ResourceTelemetrySourceStatus::Degraded;
                    state.last_error = Some(message);
                }
                self.publish_health();
                if !recoverable {
                    return Err(NativeError::CommandFailed(code.to_owned()));
                }
            }
        }
        Ok(())
    }

    /// One monitor run: start, configure, then process events until it stops (always an error).
    async fn run_attempt(&self) -> NativeError {
        let (events, mut receiver) = mpsc::unbounded_channel();
        let handle = match monitor::spawn(events) {
            Ok(handle) => Arc::new(handle),
            Err(error) => return NativeError::SpawnFailed(error.to_string()),
        };
        let generation = {
            let mut state = lock(&self.state);
            state.status = ResourceTelemetrySourceStatus::Starting;
            state.handle = Some(handle.clone());
            state.hello = None;
            state.restart_count
        };
        self.publish_health();
        // The hello comes first.
        match receiver.recv().await {
            Some(event @ MonitorEvent::Hello { .. }) => {
                let _ = self.process_event(event, generation);
            }
            _ => return NativeError::Exited,
        }
        let configured = {
            let mut control = lock(&self.control);
            let desired = control.0.clone();
            let external = lock(&self.external).clone();
            let mut ok = handle
                .send(MonitorCommand::Configure {
                    root_pid: self.root_pid,
                    sample_interval_ms: desired.sample_interval_ms.max(0) as u64,
                    external_processes: external.clone(),
                })
                .is_ok();
            if ok && desired.live_subscriber_count > 0 {
                ok = handle.send(MonitorCommand::SetStreaming(true)).is_ok();
            }
            if ok {
                control.1 = desired;
                ok = handle.send(MonitorCommand::SetExternalProcesses(external)).is_ok();
            }
            ok
        };
        if !configured {
            return NativeError::CommandFailed("configure".into());
        }
        lock(&self.state).status = ResourceTelemetrySourceStatus::Healthy;
        self.publish_health();
        while let Some(event) = receiver.recv().await {
            if let Err(error) = self.process_event(event, generation) {
                let _ = handle.send(MonitorCommand::Shutdown);
                return error;
            }
        }
        NativeError::Exited
    }
}

/// Restarts the monitor after failures. Runs until [`NativeTelemetryClient::shutdown`] aborts it.
async fn supervise(inner: Arc<Inner>) {
    let mut failures: Vec<i64> = Vec::new();
    let mut attempt: u32 = 0;
    loop {
        let error = inner.run_attempt().await;
        lock(&inner.state).handle = None;
        let now = monitor::unix_time_ms() as i64;
        let recent = retain_recent_failures(&failures, now);
        if recent.is_empty() {
            attempt = 0;
        }
        failures = recent;
        failures.push(now);
        let exhausted = failures.len() >= MAX_FAILURES_PER_WINDOW;
        {
            let mut state = lock(&inner.state);
            state.status = if exhausted {
                ResourceTelemetrySourceStatus::Unavailable
            } else {
                ResourceTelemetrySourceStatus::Degraded
            };
            state.hello = None;
            state.last_error = Some(error.to_string());
            state.restart_count += 1;
        }
        tracing::warn!(%error, exhausted, "resource monitor stopped");
        inner.publish_health();
        inner.fail_pending(&error);
        if exhausted {
            inner.retry.notified().await;
            failures.clear();
            attempt = 0;
            {
                let mut state = lock(&inner.state);
                state.status = ResourceTelemetrySourceStatus::Starting;
                state.hello = None;
                state.last_error = None;
            }
            inner.publish_health();
            continue;
        }
        let manually = tokio::select! {
            _ = tokio::time::sleep(restart_delay(attempt)) => false,
            _ = inner.retry.notified() => true,
        };
        attempt = if manually { 0 } else { attempt + 1 };
    }
}

/// A live snapshot stream that counts as a live subscriber while it exists.
struct LiveSnapshots {
    inner: Arc<Inner>,
    stream: BroadcastStream<NativeTelemetrySnapshot>,
}

impl Stream for LiveSnapshots {
    type Item = NativeTelemetrySnapshot;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match Pin::new(&mut self.stream).poll_next(cx) {
                Poll::Ready(Some(Ok(item))) => return Poll::Ready(Some(item)),
                // Lagged: the oldest snapshots were dropped (a sliding buffer).
                Poll::Ready(Some(Err(_))) => continue,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl Drop for LiveSnapshots {
    fn drop(&mut self) {
        self.inner.change_live_subscribers(-1);
    }
}

fn request_id() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn request<T>(
    inner: &Inner,
    pending: &Pending<T>,
    operation: &'static str,
    timeout: Duration,
    command: impl FnOnce(String) -> MonitorCommand,
) -> Result<T, NativeError> {
    let handle = inner.commandable_handle()?;
    let id = request_id();
    let (sender, receiver) = oneshot::channel();
    lock(pending).insert(id.clone(), sender);
    if handle.send(command(id.clone())).is_err() {
        lock(pending).remove(&id);
        return Err(NativeError::CommandFailed(operation.to_owned()));
    }
    let result = tokio::time::timeout(timeout, receiver).await;
    lock(pending).remove(&id);
    match result {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(NativeError::Exited),
        Err(_) => Err(NativeError::RequestTimedOut {
            operation,
            timeout_ms: timeout.as_millis(),
        }),
    }
}

#[async_trait]
impl NativeTelemetry for NativeTelemetryClient {
    fn snapshots(&self) -> BoxStream<'static, NativeTelemetrySnapshot> {
        let stream = BroadcastStream::new(self.inner.snapshots.subscribe());
        self.inner.change_live_subscribers(1);
        LiveSnapshots {
            inner: self.inner.clone(),
            stream,
        }
        .boxed()
    }

    async fn read_history(&self, window_ms: i64) -> Result<Vec<ResourceMonitorSnapshotEvent>, NativeError> {
        let window_ms = window_ms.max(0) as u64;
        request(
            &self.inner,
            &self.inner.pending_histories,
            "readHistory",
            HISTORY_REQUEST_TIMEOUT,
            |request_id| MonitorCommand::ReadHistory { request_id, window_ms },
        )
        .await
    }

    async fn set_external_processes(&self, processes: Vec<ResourceMonitorExternalProcess>) -> Result<(), NativeError> {
        *lock(&self.inner.external) = processes.clone();
        match self.inner.commandable_handle() {
            Ok(handle) => handle
                .send(MonitorCommand::SetExternalProcesses(processes))
                .map_err(|_| NativeError::CommandFailed("setExternalProcesses".into())),
            Err(_) => Ok(()),
        }
    }

    async fn set_host_power_state(&self, snapshot: HostPowerSnapshot) -> Result<(), NativeError> {
        self.inner.update_control(|control| {
            control.sample_interval_ms = resolve_native_sample_interval_ms(&snapshot, control.live_subscriber_count);
            control.host_power = snapshot;
        });
        Ok(())
    }

    async fn sample_now(&self) -> Result<NativeTelemetrySnapshot, NativeError> {
        request(&self.inner, &self.inner.pending_samples, "sampleNow", SAMPLE_REQUEST_TIMEOUT, |request_id| {
            MonitorCommand::SampleNow { request_id }
        })
        .await
    }

    async fn process_table(&self) -> Result<Vec<ResourceMonitorProcessTableEntry>, NativeError> {
        request(
            &self.inner,
            &self.inner.pending_tables,
            "processTable",
            PROCESS_TABLE_REQUEST_TIMEOUT,
            |request_id| MonitorCommand::ProcessTable { request_id },
        )
        .await
    }

    async fn retry(&self) -> bool {
        let accepted = {
            let state = lock(&self.inner.state);
            can_request_retry(state.status, state.handle.is_some())
        };
        if accepted {
            self.inner.retry.notify_one();
        }
        accepted
    }

    fn health(&self) -> NativeHealth {
        self.inner.health()
    }

    fn subscribe_health(&self) -> (NativeHealth, BoxStream<'static, NativeHealth>) {
        let receiver = self.inner.health_changes.subscribe();
        let latest = self.inner.health();
        (latest, BroadcastStream::new(receiver).filter_map(|item| async move { item.ok() }).boxed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ResourceTelemetrySourceStatus as S;

    fn power(f: impl FnOnce(&mut HostPowerSnapshot)) -> HostPowerSnapshot {
        let mut snapshot = HostPowerSnapshot {
            source: HostPowerSource::ElectronMain,
            idle: BackgroundBooleanState::False,
            idle_seconds: None,
            locked: BackgroundBooleanState::False,
            suspended: false,
            on_battery: BackgroundBooleanState::False,
            low_power_mode: BackgroundBooleanState::False,
            thermal_state: HostPowerThermalState::Nominal,
            stale: false,
            updated_at: DateTimeUtc::from_millis(0).unwrap(),
        };
        f(&mut snapshot);
        snapshot
    }

    #[test]
    fn keeps_a_recovery_cadence_while_suspended_and_backs_off_under_host_constraints() {
        assert_eq!(resolve_native_sample_interval_ms(&power(|p| p.suspended = true), 1), 15_000);
        assert_eq!(
            resolve_native_sample_interval_ms(&power(|p| p.locked = BackgroundBooleanState::True), 1),
            15_000
        );
        assert_eq!(
            resolve_native_sample_interval_ms(&power(|p| p.low_power_mode = BackgroundBooleanState::True), 1),
            15_000
        );
        assert_eq!(
            resolve_native_sample_interval_ms(&power(|p| p.thermal_state = HostPowerThermalState::Serious), 1),
            15_000
        );
        assert_eq!(
            resolve_native_sample_interval_ms(&power(|p| p.on_battery = BackgroundBooleanState::True), 1),
            5_000
        );
    }

    #[test]
    fn slows_background_telemetry_and_serves_live_diagnostics_at_1hz() {
        assert_eq!(resolve_native_sample_interval_ms(&power(|_| {}), 0), 5_000);
        assert_eq!(resolve_native_sample_interval_ms(&power(|_| {}), 1), 1_000);
        let unknown = unknown_power(DateTimeUtc::from_millis(0).unwrap());
        assert_eq!(resolve_native_sample_interval_ms(&unknown, 0), 5_000);
        assert_eq!(resolve_native_sample_interval_ms(&unknown, 2), 1_000);
    }

    #[test]
    fn only_accepts_retry_while_the_supervisor_is_waiting_without_a_live_sidecar() {
        assert!(can_request_retry(S::Unavailable, false));
        assert!(can_request_retry(S::Degraded, false));
        assert!(!can_request_retry(S::Degraded, true));
        assert!(!can_request_retry(S::Healthy, false));
        assert!(!can_request_retry(S::Starting, false));
    }

    #[test]
    fn keeps_on_demand_recovery_commands_available_while_a_live_sidecar_is_degraded() {
        assert!(can_command(S::Healthy, true));
        assert!(can_command(S::Degraded, true));
        assert!(!can_command(S::Degraded, false));
        assert!(!can_command(S::Starting, true));
        assert!(!can_command(S::Unavailable, true));
    }

    #[test]
    fn expires_old_failures_so_an_isolated_crash_restarts_from_the_initial_backoff() {
        assert_eq!(retain_recent_failures(&[0, 30_000, 70_000], 100_000), vec![70_000]);
        assert_eq!(restart_delay(0), Duration::from_millis(500));
        assert_eq!(restart_delay(3), Duration::from_secs(4));
        assert_eq!(restart_delay(10), MAX_RESTART_DELAY);
    }

    #[tokio::test]
    async fn samples_the_server_tree_and_streams_while_subscribed() {
        let client = NativeTelemetryClient::start(std::process::id());
        let (_, mut health) = client.subscribe_health();
        while client.health().status != S::Healthy {
            tokio::time::timeout(Duration::from_secs(5), health.next()).await.expect("healthy");
        }
        assert_eq!(client.health().sample_interval_ms, 5_000);
        let sample = client.sample_now().await.unwrap();
        assert!(sample.snapshot.processes.iter().any(|p| p.pid == i64::from(std::process::id())));
        assert_eq!(sample.generation, 0);
        let hello = client.health().hello.unwrap();
        assert_eq!(hello.sidecar_pid, i64::from(std::process::id()));

        let mut live = client.snapshots();
        assert_eq!(client.health().sample_interval_ms, 1_000);
        let streamed = tokio::time::timeout(Duration::from_secs(5), live.next()).await.unwrap().unwrap();
        assert!(streamed.snapshot.request_id.is_none());
        drop(live);
        assert_eq!(client.health().sample_interval_ms, 5_000);

        let history = client.read_history(60_000).await.unwrap();
        assert!(!history.is_empty());
        assert!(!client.retry().await);
        client.shutdown();
        assert_eq!(client.health().status, S::Stopped);
        assert!(matches!(client.sample_now().await, Err(NativeError::Unavailable(_))));
    }
}
