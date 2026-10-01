//! Builders shared by the ported TS tests (the `processSample`, `nativeSnapshot`,
//! `electronMetric` and `desktopSnapshot` helpers of the `*.test.ts` files), and fakes of the
//! native and desktop telemetry sources (the TS `layerTest`s).
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use zc_contracts::{
    BackgroundBooleanState, DateTimeUtc, DesktopElectronProcessMetric, DesktopElectronProcessType, DesktopHostTelemetrySnapshot,
    DesktopHostTelemetrySnapshotPower, HostPowerSnapshot, HostPowerSource, HostPowerThermalState, JsNumber, Lit1, Lit3, LitDesktopTelemetry, LitSnapshot,
    ResourceMonitorExternalProcess, ResourceMonitorProcessSample, ResourceMonitorProcessSampleIoSemantics, ResourceMonitorProcessTableEntry,
    ResourceMonitorSnapshotEvent, ResourceTelemetrySourceStatus,
};
use zc_telemetry::desktop::{DesktopHealth, DesktopTelemetry};
use zc_telemetry::native::{NativeError, NativeHealth, NativeTelemetry, NativeTelemetrySnapshot};

pub fn ms(iso: &str) -> i64 {
    DateTimeUtc::parse(iso).unwrap().as_millis()
}

pub fn date(ms: i64) -> DateTimeUtc {
    DateTimeUtc::from_millis(ms).unwrap()
}

pub fn process_sample(pid: i64, ppid: i64, start_time_ms: i64) -> ResourceMonitorProcessSample {
    ResourceMonitorProcessSample {
        pid,
        ppid,
        start_time_ms,
        run_time_ms: 1_000,
        name: format!("process-{pid}"),
        command: format!("process-{pid}"),
        status: "Running".into(),
        cpu_percent: JsNumber(0.0),
        cpu_time_ms: 0,
        resident_bytes: 1_024,
        virtual_bytes: 2_048,
        io_read_bytes: 0,
        io_write_bytes: 0,
        io_semantics: ResourceMonitorProcessSampleIoSemantics::Storage,
    }
}

pub fn with(mut sample: ResourceMonitorProcessSample, f: impl FnOnce(&mut ResourceMonitorProcessSample)) -> ResourceMonitorProcessSample {
    f(&mut sample);
    sample
}

pub fn native_snapshot(sampled_at_unix_ms: i64, processes: Vec<ResourceMonitorProcessSample>, sequence: i64) -> ResourceMonitorSnapshotEvent {
    ResourceMonitorSnapshotEvent {
        version: Lit3,
        r#type: LitSnapshot,
        sequence,
        sampled_at_unix_ms,
        collection_duration_micros: 250,
        scanned_process_count: processes.len() as i64,
        retained_process_count: processes.len() as i64,
        inaccessible_process_count: 0,
        request_id: None,
        external_processes: None,
        processes,
    }
}

pub fn electron_metric(pid: i64, creation_time_ms: i64, kind: DesktopElectronProcessType) -> DesktopElectronProcessMetric {
    DesktopElectronProcessMetric {
        pid,
        creation_time_ms,
        r#type: kind,
        name: None,
        service_name: None,
        cpu_percent: JsNumber(0.0),
        cumulative_cpu_seconds: None,
        idle_wakeups_per_second: JsNumber(0.0),
        working_set_bytes: 1_024,
        peak_working_set_bytes: 2_048,
    }
}

pub fn desktop_power(sampled_at_unix_ms: i64) -> DesktopHostTelemetrySnapshotPower {
    DesktopHostTelemetrySnapshotPower {
        source: HostPowerSource::ElectronMain,
        idle: BackgroundBooleanState::False,
        idle_seconds: Some(JsNumber(0.0)),
        locked: BackgroundBooleanState::False,
        suspended: false,
        on_battery: BackgroundBooleanState::False,
        low_power_mode: BackgroundBooleanState::Unknown,
        thermal_state: HostPowerThermalState::Nominal,
        stale: false,
        updated_at: date(sampled_at_unix_ms),
    }
}

pub fn desktop_snapshot(sampled_at_unix_ms: i64, electron_processes: Vec<DesktopElectronProcessMetric>) -> DesktopHostTelemetrySnapshot {
    DesktopHostTelemetrySnapshot {
        version: Lit1,
        r#type: LitDesktopTelemetry,
        sequence: 1,
        sampled_at_unix_ms,
        electron_pid: electron_processes.first().map_or(10_000, |p| p.pid),
        power: desktop_power(sampled_at_unix_ms),
        speed_limit_percent: None,
        electron_processes,
    }
}

pub fn healthy(sample_interval_ms: i64) -> NativeHealth {
    NativeHealth {
        status: ResourceTelemetrySourceStatus::Healthy,
        hello: None,
        last_sample_at: None,
        last_error: None,
        restart_count: 0,
        sample_interval_ms,
    }
}

type SampleFn = Box<dyn Fn(usize) -> Option<Result<NativeTelemetrySnapshot, NativeError>> + Send + Sync>;

/// `NativeTelemetryClient.layerTest`: unavailable unless told otherwise.
pub struct FakeNative {
    pub health: Mutex<NativeHealth>,
    pub health_changes: broadcast::Sender<NativeHealth>,
    pub snapshots: broadcast::Sender<NativeTelemetrySnapshot>,
    /// `None` = never answers.
    pub sample: SampleFn,
    pub sample_calls: Mutex<usize>,
    pub history: Mutex<Option<Vec<ResourceMonitorSnapshotEvent>>>,
    pub external: Mutex<Vec<ResourceMonitorExternalProcess>>,
    pub retries: Mutex<usize>,
    pub retry_accepted: bool,
}

impl FakeNative {
    pub fn new(health: NativeHealth) -> Self {
        Self {
            health: Mutex::new(health),
            health_changes: broadcast::channel(4).0,
            snapshots: broadcast::channel(16).0,
            sample: Box::new(|_| Some(Err(NativeError::Unavailable("No resource monitor sample was configured for this test.".into())))),
            sample_calls: Mutex::new(0),
            history: Mutex::new(None),
            external: Mutex::new(Vec::new()),
            retries: Mutex::new(0),
            retry_accepted: false,
        }
    }

    pub fn sampling(mut self, f: impl Fn(usize) -> Option<Result<NativeTelemetrySnapshot, NativeError>> + Send + Sync + 'static) -> Self {
        self.sample = Box::new(f);
        self
    }

    pub fn set_health(&self, f: impl FnOnce(&mut NativeHealth)) {
        let mut health = self.health.lock().unwrap();
        f(&mut health);
        let _ = self.health_changes.send(health.clone());
    }
}

#[async_trait]
impl NativeTelemetry for FakeNative {
    fn snapshots(&self) -> BoxStream<'static, NativeTelemetrySnapshot> {
        BroadcastStream::new(self.snapshots.subscribe()).filter_map(|s| async move { s.ok() }).boxed()
    }

    async fn read_history(&self, _window_ms: i64) -> Result<Vec<ResourceMonitorSnapshotEvent>, NativeError> {
        self.history
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| NativeError::Unavailable("No resource monitor history was configured for this test.".into()))
    }

    async fn set_external_processes(&self, processes: Vec<ResourceMonitorExternalProcess>) -> Result<(), NativeError> {
        *self.external.lock().unwrap() = processes;
        Ok(())
    }

    async fn set_host_power_state(&self, _snapshot: HostPowerSnapshot) -> Result<(), NativeError> {
        Ok(())
    }

    async fn sample_now(&self) -> Result<NativeTelemetrySnapshot, NativeError> {
        let call = {
            let mut calls = self.sample_calls.lock().unwrap();
            *calls += 1;
            *calls - 1
        };
        match (self.sample)(call) {
            Some(result) => result,
            None => futures::future::pending().await,
        }
    }

    async fn process_table(&self) -> Result<Vec<ResourceMonitorProcessTableEntry>, NativeError> {
        Err(NativeError::Unavailable(
            "No resource monitor process table was configured for this test.".into(),
        ))
    }

    async fn retry(&self) -> bool {
        *self.retries.lock().unwrap() += 1;
        self.retry_accepted
    }

    fn health(&self) -> NativeHealth {
        self.health.lock().unwrap().clone()
    }

    fn subscribe_health(&self) -> (NativeHealth, BoxStream<'static, NativeHealth>) {
        let receiver = self.health_changes.subscribe();
        (self.health(), BroadcastStream::new(receiver).filter_map(|s| async move { s.ok() }).boxed())
    }
}

/// `DesktopTelemetryReceiver.layerTest`.
pub struct FakeDesktop {
    pub latest: Option<DesktopHostTelemetrySnapshot>,
    pub health: DesktopHealth,
    pub demand: Arc<Mutex<Vec<bool>>>,
}

impl FakeDesktop {
    pub fn new(latest: Option<DesktopHostTelemetrySnapshot>) -> Self {
        Self {
            latest,
            health: DesktopHealth {
                status: ResourceTelemetrySourceStatus::Unavailable,
                last_sample_at: None,
                last_error: Some("Desktop telemetry test implementation is unavailable.".into()),
            },
            demand: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl DesktopTelemetry for FakeDesktop {
    fn latest(&self) -> Option<DesktopHostTelemetrySnapshot> {
        self.latest.clone()
    }

    fn health(&self) -> DesktopHealth {
        self.health.clone()
    }

    fn set_diagnostics_demand(&self, enabled: bool) {
        self.demand.lock().unwrap().push(enabled);
    }
}

pub fn generation(snapshot: ResourceMonitorSnapshotEvent, generation: i64) -> NativeTelemetrySnapshot {
    NativeTelemetrySnapshot { generation, snapshot }
}
