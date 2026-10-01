//! `resourceTelemetry/ResourceTelemetry.test.ts`, `diagnostics/ProcessDiagnostics.test.ts` and
//! `diagnostics/ProcessResourceMonitor.test.ts`, over fake native and desktop sources.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use futures::StreamExt;
use zc_contracts::{
    BackgroundBooleanState, DateTimeUtc, DesktopElectronProcessType, DesktopHostTelemetrySnapshot, EOption, JsNumber, ResourceMonitorExternalProcess,
    ResourceMonitorSnapshotEvent, ResourceTelemetryHealth, ResourceTelemetryHistory, ResourceTelemetryHistoryBucket, ResourceTelemetryIoSemantics,
    ResourceTelemetryProcessCategory, ResourceTelemetryProcessIdentity, ResourceTelemetryProcessSummary, ResourceTelemetrySourceHealth,
    ResourceTelemetrySourceStatus, ServerProcessResourceHistoryFailureTag, ServerProcessSignal, ServerSignalProcessInput,
};
use zc_telemetry::diagnostics::{project_process_resource_history, read_process_diagnostics, signal_process};
use zc_telemetry::history::HistoryWithLegacyBuckets;
use zc_telemetry::native::{MonitorHello, NativeError};
use zc_telemetry::{AttributionRecord, ResourceAttribution, ResourceTelemetry};

fn pid() -> i64 {
    i64::from(std::process::id())
}

fn telemetry_snapshot(sequence: i64, sampled_at: i64, child_cpu: i64, child_write: i64) -> ResourceMonitorSnapshotEvent {
    let mut s = native_snapshot(
        sampled_at,
        vec![
            with(process_sample(pid(), 1, 100), |p| p.cpu_time_ms = sequence * 10),
            with(process_sample(4_242, pid(), 200), |p| {
                p.name = "codex".into();
                p.command = "codex app-server".into();
                p.cpu_time_ms = child_cpu;
                p.io_write_bytes = child_write;
            }),
            with(process_sample(5_000, 1, 300), |p| {
                p.name = "electron".into();
                p.command = "electron".into();
                p.cpu_time_ms = sequence * 20;
            }),
            with(process_sample(9_000, pid(), 400), |p| {
                p.name = "t3-resource-monitor".into();
                p.command = "t3-resource-monitor".into();
                p.cpu_time_ms = sequence * 5;
            }),
        ],
        sequence,
    );
    s.collection_duration_micros = 300;
    s.scanned_process_count = 80;
    s.inaccessible_process_count = 1;
    s
}

fn electron_desktop(sampled_at: i64) -> DesktopHostTelemetrySnapshot {
    let mut metric = electron_metric(5_000, 300, DesktopElectronProcessType::Browser);
    metric.name = Some("electron".into());
    metric.cpu_percent = JsNumber(2.0);
    metric.cumulative_cpu_seconds = Some(JsNumber(0.02));
    metric.idle_wakeups_per_second = JsNumber(3.0);
    metric.working_set_bytes = 4_096;
    metric.peak_working_set_bytes = 8_192;
    let mut desktop = desktop_snapshot(sampled_at, vec![metric]);
    desktop.power.idle_seconds = Some(JsNumber(2.0));
    desktop.power.on_battery = BackgroundBooleanState::True;
    desktop.power.thermal_state = zc_contracts::HostPowerThermalState::Fair;
    desktop.speed_limit_percent = Some(JsNumber(90.0));
    desktop
}

async fn start(native: Arc<FakeNative>, desktop: FakeDesktop) -> Arc<ResourceTelemetry> {
    let telemetry = ResourceTelemetry::start(native, Arc::new(desktop), ResourceAttribution::new(), pid());
    // The initial Electron root is handed to the native source in the background.
    tokio::time::sleep(Duration::from_millis(5)).await;
    telemetry
}

#[tokio::test]
async fn enables_live_native_and_electron_collection_only_while_changes_are_retained() {
    let now = DateTimeUtc::now().as_millis();
    let sample = telemetry_snapshot(1, now, 100, 1_000);
    let native = Arc::new(FakeNative::new(healthy(1_000)).sampling(move |_| Some(Ok(generation(sample.clone(), 0)))));
    let desktop = FakeDesktop::new(Some(electron_desktop(now)));
    let demand = desktop.demand.clone();
    let telemetry = start(native, desktop).await;
    let (_, mut changes) = telemetry.subscribe();
    let first = tokio::time::timeout(Duration::from_secs(1), changes.next()).await.unwrap();
    assert!(first.is_some());
    drop(changes);
    assert_eq!(*demand.lock().unwrap(), [true, false]);
}

#[tokio::test]
async fn releases_live_demand_when_the_subscriber_leaves_before_a_sample() {
    let native = Arc::new(FakeNative::new(healthy(1_000)).sampling(|_| None));
    let desktop = FakeDesktop::new(None);
    let demand = desktop.demand.clone();
    let telemetry = start(native, desktop).await;
    let (_, mut changes) = telemetry.subscribe();
    assert!(tokio::time::timeout(Duration::from_millis(10), changes.next()).await.is_err());
    drop(changes);
    assert_eq!(*demand.lock().unwrap(), [true, false]);
}

#[tokio::test]
async fn attributes_an_initial_electron_root_from_the_identity_recorded_by_the_native_snapshot() {
    let now = DateTimeUtc::now().as_millis();
    let mut sample = telemetry_snapshot(1, now, 100, 1_000);
    sample.external_processes = Some(vec![ResourceMonitorExternalProcess {
        pid: 5_000,
        start_time_ms: Some(300),
    }]);
    let native = Arc::new(FakeNative::new(healthy(1_000)).sampling(move |_| Some(Ok(generation(sample.clone(), 0)))));
    let mut desktop = electron_desktop(now);
    desktop.electron_processes.clear();
    let telemetry = start(native, FakeDesktop::new(Some(desktop))).await;
    let snapshot = telemetry.refresh().await.unwrap();
    let electron = snapshot.processes.iter().find(|p| p.identity.pid == 5_000).unwrap();
    assert_eq!(electron.category, ResourceTelemetryProcessCategory::ElectronMain);
    assert_eq!(snapshot.groups.electron.process_starts, 1);
    assert_eq!(snapshot.groups.backend.process_starts, 3);
}

#[tokio::test]
async fn rejects_buffered_snapshots_from_an_earlier_sidecar_generation() {
    let started = DateTimeUtc::now().as_millis();
    let stale = telemetry_snapshot(100, started + 1_000, 100, 1_000);
    let current = telemetry_snapshot(1, started + 2_000, 200, 2_000);
    let mut health = healthy(1_000);
    health.restart_count = 1;
    let native = Arc::new(FakeNative::new(health).sampling(|_| None));
    let telemetry = start(native.clone(), FakeDesktop::new(None)).await;
    let (_, mut changes) = telemetry.subscribe();
    tokio::task::yield_now().await;
    native.snapshots.send(generation(stale, 0)).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(telemetry.latest().health.scanned_process_count, 0);
    native.snapshots.send(generation(current.clone(), 1)).unwrap();
    let received = tokio::time::timeout(Duration::from_secs(1), changes.next()).await.unwrap().unwrap();
    assert_eq!(received.read_at.as_millis(), current.sampled_at_unix_ms);
}

#[tokio::test]
async fn subscribes_atomically_after_a_desktop_update() {
    let started = DateTimeUtc::now().as_millis();
    let native = Arc::new(FakeNative::new(healthy(1_000)).sampling(|_| None));
    let telemetry = start(native, FakeDesktop::new(None)).await;
    let next = electron_desktop(started + 1_000);
    telemetry.ingest_desktop(next.clone()).await;
    let (latest, _changes) = telemetry.subscribe();
    assert_eq!(latest.read_at.as_millis(), next.sampled_at_unix_ms);
    assert_eq!(latest.power.on_battery, BackgroundBooleanState::True);
    assert_eq!(latest.speed_limit_percent, EOption(Some(JsNumber(90.0))));
}

#[tokio::test]
async fn combines_native_electron_attribution_retry_and_history_data() {
    // In the past, so the history read "now" sees every sample (the TS test moves a test clock).
    let started = DateTimeUtc::now().as_millis() - 10_000;
    let samples = [
        telemetry_snapshot(1, started, 100, 1_000),
        telemetry_snapshot(2, started + 1_000, 350, 5_000),
        telemetry_snapshot(1, started + 2_000, 500, 7_000),
    ];
    let mut health = healthy(1_000);
    health.hello = Some(MonitorHello {
        sidecar_version: "0.1.0".into(),
        sidecar_pid: 9_000,
    });
    health.last_sample_at = Some(date(started));
    health.restart_count = 2;
    let replay = samples.clone();
    let mut fake = FakeNative::new(health).sampling(move |call| {
        let index = call.min(replay.len() - 1);
        Some(Ok(generation(replay[index].clone(), if index == 2 { 3 } else { 2 })))
    });
    fake.retry_accepted = true;
    *fake.history.lock().unwrap() = Some(samples[..2].to_vec());
    let native = Arc::new(fake);
    let mut desktop = FakeDesktop::new(Some(electron_desktop(started)));
    desktop.health = zc_telemetry::desktop::DesktopHealth {
        status: ResourceTelemetrySourceStatus::Healthy,
        last_sample_at: Some(date(started)),
        last_error: None,
    };
    let attribution = ResourceAttribution::new();
    let telemetry = ResourceTelemetry::start(native.clone(), Arc::new(desktop), attribution.clone(), pid());
    tokio::time::sleep(Duration::from_millis(5)).await;

    assert_eq!(
        *native.external.lock().unwrap(),
        [ResourceMonitorExternalProcess {
            pid: 5_000,
            start_time_ms: Some(300)
        }]
    );
    attribution.record(AttributionRecord {
        component: "provider-event-log".into(),
        operation: "append".into(),
        logical_write_bytes: Some(512.0),
        count: Some(2.0),
        duration_ms: Some(4.0),
        ..Default::default()
    });
    let first = telemetry.refresh().await.unwrap();
    assert_eq!(first.groups.backend.process_count, 2);
    assert_eq!(first.groups.electron.process_count, 1);
    assert_eq!(first.groups.monitor.process_count, 1);
    assert_eq!(first.power.on_battery, BackgroundBooleanState::True);
    assert_eq!(first.speed_limit_percent, EOption(Some(JsNumber(90.0))));
    assert_eq!(first.attribution.entries.len(), 1);
    assert_eq!(first.attribution.entries[0].component, "provider-event-log");
    assert_eq!(first.attribution.entries[0].logical_read_bytes, 0);
    assert_eq!(first.attribution.entries[0].logical_write_bytes, 512);
    assert_eq!(first.attribution.entries[0].count, 2);
    assert_eq!(first.attribution.entries[0].duration_ms, 4);

    let second = telemetry.refresh().await.unwrap();
    let codex = second.processes.iter().find(|p| p.identity.pid == 4_242).unwrap();
    assert_eq!(codex.cpu_percent.0, 25.0);
    assert_eq!(codex.io_write_bytes_per_second.0, 4_000.0);
    assert_eq!(second.groups.backend.io_write_bytes, 4_000);
    assert_eq!(second.health.collection_duration_micros, 300);
    assert_eq!(second.health.scanned_process_count, 80);
    assert_eq!(second.health.inaccessible_process_count, 1);

    let history = telemetry.read_history(60_000, 10_000).await.history;
    assert!(history.retained_sample_count > 0);
    let codex = history.top_processes.iter().find(|p| p.identity.pid == 4_242).unwrap();
    assert_eq!(codex.sample_count, 2);
    assert_eq!(codex.cpu_time_ms, 250);
    assert_eq!(codex.io_write_bytes, 4_000);
    assert_eq!(history.buckets.iter().map(|b| b.io_write_bytes).sum::<i64>(), 4_000);

    let retry = telemetry.retry().await;
    assert!(retry.accepted);
    assert_eq!(*native.retries.lock().unwrap(), 1);

    native.set_health(|h| {
        h.hello.as_mut().unwrap().sidecar_pid = 9_001;
        h.restart_count = 3;
    });
    let restarted = telemetry.refresh().await.unwrap();
    assert_eq!(restarted.read_at.as_millis(), started + 2_000);
    assert_eq!(restarted.health.sidecar_pid, EOption(Some(9_001)));

    native.set_health(|h| {
        h.status = ResourceTelemetrySourceStatus::Degraded;
        h.last_error = Some("collector exited".into());
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let update = telemetry.latest();
    assert_eq!(update.health.native.status, ResourceTelemetrySourceStatus::Degraded);
    assert_eq!(update.health.native.last_error, EOption(Some("collector exited".into())));
    let degraded = telemetry.read_history(60_000, 10_000).await.history;
    assert_eq!(degraded.health.native.status, ResourceTelemetrySourceStatus::Degraded);
}

// ProcessDiagnostics.test.ts

fn diagnostics_sample(processes: Vec<zc_contracts::ResourceMonitorProcessSample>) -> ResourceMonitorSnapshotEvent {
    native_snapshot(ms("2026-05-05T10:00:00.000Z"), processes, 1)
}

fn agent(ppid: i64) -> zc_contracts::ResourceMonitorProcessSample {
    with(process_sample(4_242, ppid, 2_000), |p| {
        p.run_time_ms = 4_000;
        p.name = "agent".into();
        p.command = "codex app-server".into();
        p.cpu_percent = JsNumber(1.5);
        p.cpu_time_ms = 60;
        p.resident_bytes = 2_048;
        p.virtual_bytes = 4_096;
        p.io_read_bytes = 300;
        p.io_write_bytes = 400;
    })
}

async fn diagnostics_telemetry(snapshot: ResourceMonitorSnapshotEvent, desktop: Option<DesktopHostTelemetrySnapshot>) -> Arc<ResourceTelemetry> {
    let mut health = healthy(1_000);
    health.last_sample_at = Some(date(snapshot.sampled_at_unix_ms));
    let native = Arc::new(FakeNative::new(health).sampling(move |_| Some(Ok(generation(snapshot.clone(), 0)))));
    start(native, FakeDesktop::new(desktop)).await
}

type Signals = Arc<Mutex<Vec<(i64, ServerProcessSignal)>>>;

fn recording_killer(signals: &Signals) -> impl Fn(i64, ServerProcessSignal) -> std::io::Result<()> + Send + Sync {
    let signals = signals.clone();
    move |pid, signal| {
        signals.lock().unwrap().push((pid, signal));
        Ok(())
    }
}

#[tokio::test]
async fn projects_live_process_data_from_resource_telemetry() {
    let server = with(process_sample(pid(), 1, 1_000), |p| {
        p.run_time_ms = 60_000;
        p.name = "node".into();
        p.command = "t3 server".into();
    });
    let telemetry = diagnostics_telemetry(diagnostics_sample(vec![server, agent(pid())]), None).await;
    let diagnostics = read_process_diagnostics(&telemetry).await;
    assert_eq!(diagnostics.processes.iter().map(|p| p.pid).collect::<Vec<_>>(), [4242]);
    assert_eq!(diagnostics.processes[0].start_time_ms, 2_000);
    assert_eq!(diagnostics.processes[0].cpu_percent.0, 1.5);
    assert_eq!(diagnostics.processes[0].rss_bytes, 2_048);
    assert_eq!(diagnostics.processes[0].elapsed, "0:04");
    assert_eq!(diagnostics.processes[0].depth, 0);
    assert_eq!(diagnostics.server_pid, pid());
    assert_eq!(diagnostics.error, EOption(None));
}

#[tokio::test]
async fn rejects_stale_process_identities_before_signaling() {
    let telemetry = diagnostics_telemetry(diagnostics_sample(vec![]), None).await;
    let signals = Signals::default();
    let input = ServerSignalProcessInput {
        pid: 4_242,
        start_time_ms: 2_000,
        signal: ServerProcessSignal::SIGINT,
    };
    let result = signal_process(&telemetry, &input, &recording_killer(&signals)).await;
    assert!(!result.signaled);
    assert_eq!(
        result.message,
        EOption(Some("Process 4242 no longer matches the selected process identity.".into()))
    );
    assert!(signals.lock().unwrap().is_empty());
}

#[tokio::test]
async fn refuses_to_signal_when_a_fresh_identity_check_cannot_be_collected() {
    let native = Arc::new(FakeNative::new(healthy(1_000)).sampling(|_| Some(Err(NativeError::Unavailable("collector unavailable".into())))));
    let telemetry = start(native, FakeDesktop::new(None)).await;
    let signals = Signals::default();
    let input = ServerSignalProcessInput {
        pid: 4_242,
        start_time_ms: 2_000,
        signal: ServerProcessSignal::SIGINT,
    };
    let result = signal_process(&telemetry, &input, &recording_killer(&signals)).await;
    assert_eq!(
        result.message,
        EOption(Some("Could not refresh process 4242; refusing to signal a stale identity.".into()))
    );
    assert!(!result.signaled);
}

#[tokio::test]
async fn refuses_to_signal_the_server_and_signals_a_matching_descendant() {
    let telemetry = diagnostics_telemetry(diagnostics_sample(vec![process_sample(pid(), 1, 1_000), agent(pid())]), None).await;
    let signals = Signals::default();
    let killer = recording_killer(&signals);
    let own = signal_process(
        &telemetry,
        &ServerSignalProcessInput {
            pid: pid(),
            start_time_ms: 1_000,
            signal: ServerProcessSignal::SIGKILL,
        },
        &killer,
    )
    .await;
    assert_eq!(own.message, EOption(Some("Refusing to signal the T3 server process.".into())));
    let child = signal_process(
        &telemetry,
        &ServerSignalProcessInput {
            pid: 4_242,
            start_time_ms: 2_000,
            signal: ServerProcessSignal::SIGINT,
        },
        &killer,
    )
    .await;
    assert!(child.signaled);
    assert_eq!(child.message, EOption(None));
    assert_eq!(*signals.lock().unwrap(), [(4_242, ServerProcessSignal::SIGINT)]);
    let failing = |_: i64, _: ServerProcessSignal| Err(std::io::Error::from_raw_os_error(libc_esrch()));
    let failed = signal_process(
        &telemetry,
        &ServerSignalProcessInput {
            pid: 4_242,
            start_time_ms: 2_000,
            signal: ServerProcessSignal::SIGKILL,
        },
        &failing,
    )
    .await;
    assert_eq!(failed.message, EOption(Some("Failed to signal process 4242 with SIGKILL.".into())));
}

fn libc_esrch() -> i32 {
    3
}

#[tokio::test]
async fn rejects_electron_processes_as_signal_targets() {
    let sampled_at = ms("2026-05-05T10:00:00.000Z");
    let electron = with(agent(1), |p| {
        p.name = "electron".into();
        p.command = "electron".into();
    });
    let mut metric = electron_metric(4_242, 2_000, DesktopElectronProcessType::Browser);
    metric.name = Some("electron".into());
    metric.cpu_percent = JsNumber(1.5);
    metric.working_set_bytes = 2_048;
    metric.peak_working_set_bytes = 2_048;
    let telemetry = diagnostics_telemetry(diagnostics_sample(vec![electron]), Some(desktop_snapshot(sampled_at, vec![metric]))).await;
    let signals = Signals::default();
    let result = signal_process(
        &telemetry,
        &ServerSignalProcessInput {
            pid: 4_242,
            start_time_ms: 2_000,
            signal: ServerProcessSignal::SIGKILL,
        },
        &recording_killer(&signals),
    )
    .await;
    assert!(!result.signaled);
    assert_eq!(result.message, EOption(Some("Process 4242 is not a signalable T3 backend descendant.".into())));
    let diagnostics = read_process_diagnostics(&telemetry).await;
    assert!(diagnostics.processes.is_empty());
    assert_eq!(diagnostics.process_count, 0);
    assert_eq!(diagnostics.total_cpu_percent.0, 0.0);
    assert_eq!(diagnostics.total_rss_bytes, 0);
}

// ProcessResourceMonitor.test.ts

#[test]
fn projects_resource_telemetry_history_into_the_legacy_diagnostics_contract() {
    let read_at = DateTimeUtc::parse("2026-05-05T10:00:00.000Z").unwrap();
    let started_at = DateTimeUtc::parse("2026-05-05T09:59:50.000Z").unwrap();
    let first_seen = DateTimeUtc::parse("2026-05-05T09:59:55.000Z").unwrap();
    let bucket = |avg: f64, max: f64, count: i64| ResourceTelemetryHistoryBucket {
        started_at,
        ended_at: read_at,
        avg_cpu_percent: JsNumber(avg),
        max_cpu_percent: JsNumber(max),
        max_rss_bytes: 4_096,
        io_read_bytes: 1_024,
        io_write_bytes: 2_048,
        max_process_count: count,
    };
    let summary = |pid: i64, start: i64, name: &str, category, cpu: (f64, f64, f64), cpu_time: i64, rss: (i64, i64)| ResourceTelemetryProcessSummary {
        identity: ResourceTelemetryProcessIdentity { pid, start_time_ms: start },
        ppid: 1,
        depth: 0,
        name: name.into(),
        command: if name == "node" { "t3 server".into() } else { name.into() },
        category,
        first_seen_at: first_seen,
        last_seen_at: read_at,
        current_cpu_percent: JsNumber(cpu.0),
        avg_cpu_percent: JsNumber(cpu.1),
        max_cpu_percent: JsNumber(cpu.2),
        cpu_time_ms: cpu_time,
        current_rss_bytes: rss.0,
        peak_rss_bytes: rss.1,
        io_read_bytes: 1_024,
        io_write_bytes: 2_048,
        io_semantics: ResourceTelemetryIoSemantics::Storage,
        sample_count: 2,
    };
    let source = |status, error: Option<&str>| ResourceTelemetrySourceHealth {
        status,
        last_sample_at: EOption(Some(read_at)),
        last_error: EOption(error.map(str::to_owned)),
    };
    let history = HistoryWithLegacyBuckets {
        history: ResourceTelemetryHistory {
            read_at,
            window_ms: 60_000,
            bucket_ms: 10_000,
            sample_interval_ms: 1_000,
            retained_sample_count: 2,
            buckets: vec![bucket(15.0, 25.0, 2)],
            top_processes: vec![
                summary(
                    pid(),
                    100,
                    "node",
                    ResourceTelemetryProcessCategory::Server,
                    (5.0, 4.0, 8.0),
                    1_500,
                    (2_048, 4_096),
                ),
                summary(
                    5_000,
                    200,
                    "electron",
                    ResourceTelemetryProcessCategory::ElectronMain,
                    (50.0, 40.0, 80.0),
                    15_000,
                    (20_480, 40_960),
                ),
            ],
            health: ResourceTelemetryHealth {
                native: source(ResourceTelemetrySourceStatus::Degraded, Some("collector stalled")),
                desktop: source(ResourceTelemetrySourceStatus::Healthy, None),
                sidecar_version: EOption(Some("0.1.0".into())),
                sidecar_pid: EOption(Some(9_000)),
                restart_count: 1,
                collection_duration_micros: 250,
                scanned_process_count: 80,
                retained_process_count: 2,
                inaccessible_process_count: 0,
            },
        },
        legacy_backend_buckets: vec![bucket(5.0, 8.0, 1)],
    };
    let result = project_process_resource_history(history);
    assert_eq!(result.total_cpu_seconds_approx.0, 1.5);
    assert_eq!(result.top_processes.len(), 1);
    let top = &result.top_processes[0];
    assert_eq!(top.process_key, format!("{}:100", pid()));
    assert_eq!(top.pid, pid());
    assert_eq!(top.command, "t3 server");
    assert!(top.is_server_root);
    assert_eq!(top.first_seen_at, first_seen);
    assert_eq!(top.cpu_seconds_approx.0, 1.5);
    assert_eq!(top.max_rss_bytes, 4_096);
    assert_eq!(top.sample_count, 2);
    assert_eq!(result.buckets[0].avg_cpu_percent.0, 5.0);
    assert_eq!(result.buckets[0].max_cpu_percent.0, 8.0);
    assert_eq!(result.buckets[0].max_rss_bytes, 4_096);
    assert_eq!(result.buckets[0].max_process_count, 1);
    let error = result.error.0.unwrap();
    assert_eq!(error.failure_tag, ServerProcessResourceHistoryFailureTag::ProcessDiagnosticsQueryFailedError);
    assert_eq!(error.message, "collector stalled");
}
