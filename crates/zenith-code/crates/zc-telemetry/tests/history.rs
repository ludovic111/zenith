//! `resourceTelemetry/ResourceTelemetryHistory.test.ts`.

mod common;

use common::*;
use zc_contracts::{
    DesktopElectronProcessType, DesktopHostTelemetrySnapshot, EOption, JsNumber, ResourceMonitorExternalProcess, ResourceMonitorSnapshotEvent,
    ResourceTelemetryHealth, ResourceTelemetryProcessCategory as C, ResourceTelemetrySourceHealth, ResourceTelemetrySourceStatus,
};
use zc_telemetry::history::{build_history, normalize_history_input, BuildHistoryInput};

const SERVER_PID: i64 = 100;
const ELECTRON_PID: i64 = 200;
const CHILD_PID: i64 = 300;

fn started() -> i64 {
    ms("2026-06-17T12:00:00.000Z")
}

fn snapshot(sequence: i64, sampled_at: i64, child_cpu: i64, child_write: i64) -> ResourceMonitorSnapshotEvent {
    let mut s = native_snapshot(
        sampled_at,
        vec![
            process_sample(SERVER_PID, 1, 10),
            with(process_sample(ELECTRON_PID, 1, 20), |p| {
                p.name = "electron".into();
                p.command = "electron".into();
            }),
            with(process_sample(CHILD_PID, SERVER_PID, 30), |p| {
                p.name = "codex".into();
                p.command = "codex app-server".into();
                p.cpu_time_ms = child_cpu;
                p.io_write_bytes = child_write;
            }),
        ],
        sequence,
    );
    s.collection_duration_micros = 100;
    s
}

fn health() -> ResourceTelemetryHealth {
    let source = || ResourceTelemetrySourceHealth {
        status: ResourceTelemetrySourceStatus::Healthy,
        last_sample_at: EOption(None),
        last_error: EOption(None),
    };
    ResourceTelemetryHealth {
        native: source(),
        desktop: source(),
        sidecar_version: EOption(Some("0.1.0".into())),
        sidecar_pid: EOption(Some(400)),
        restart_count: 0,
        collection_duration_micros: 100,
        scanned_process_count: 3,
        retained_process_count: 3,
        inaccessible_process_count: 0,
    }
}

fn desktop() -> DesktopHostTelemetrySnapshot {
    let mut metric = electron_metric(ELECTRON_PID, 20, DesktopElectronProcessType::Browser);
    metric.cpu_percent = JsNumber(999.0);
    metric.idle_wakeups_per_second = JsNumber(999.0);
    metric.working_set_bytes = 999_999;
    metric.peak_working_set_bytes = 999_999;
    desktop_snapshot(started() + 1_000, vec![metric])
}

fn history(
    read_at: i64,
    window_ms: i64,
    sidecar_pid: Option<i64>,
    desktop: Option<&DesktopHostTelemetrySnapshot>,
    snapshots: &[ResourceMonitorSnapshotEvent],
) -> zc_contracts::ResourceTelemetryHistory {
    build_history(BuildHistoryInput {
        read_at_ms: read_at,
        window_ms,
        bucket_ms: window_ms,
        sample_interval_ms: 1_000,
        server_pid: SERVER_PID,
        sidecar_pid,
        desktop_snapshot: desktop,
        snapshots,
        health: health(),
    })
    .history
}

fn top(history: &zc_contracts::ResourceTelemetryHistory, pid: i64) -> &zc_contracts::ResourceTelemetryProcessSummary {
    history.top_processes.iter().find(|p| p.identity.pid == pid).unwrap()
}

#[test]
fn normalizes_query_bounds_before_requesting_native_history() {
    assert_eq!(normalize_history_input(0, 0), (1_000, 1_000));
    assert_eq!(normalize_history_input(10 * 3_600_000, 5_000_000), (3_600_000, 3_600_000));
}

#[test]
fn replays_native_snapshots_on_demand_without_applying_current_electron_metrics() {
    let desktop = desktop();
    let h = history(
        started() + 2_000,
        10_000,
        Some(400),
        Some(&desktop),
        &[snapshot(1, started(), 100, 1_000), snapshot(2, started() + 1_000, 350, 5_000)],
    );
    let child = top(&h, CHILD_PID);
    let electron = top(&h, ELECTRON_PID);
    assert_eq!(child.sample_count, 2);
    assert_eq!(child.cpu_time_ms, 250);
    assert_eq!(child.io_write_bytes, 4_000);
    assert_eq!(electron.category, C::ElectronMain);
    assert_eq!(electron.current_rss_bytes, 1_024);
    assert_eq!(h.buckets.iter().map(|b| b.io_write_bytes).sum::<i64>(), 4_000);
}

#[test]
fn uses_observed_rss_for_the_history_window_peak_instead_of_the_lifetime_process_peak() {
    let mut first = snapshot(1, started(), 100, 1_000);
    let mut second = snapshot(2, started() + 1_000, 200, 2_000);
    first.processes[2].resident_bytes = 2_000;
    second.processes[2].resident_bytes = 3_000;
    let h = history(started() + 2_000, 10_000, None, None, &[first, second]);
    assert_eq!(top(&h, CHILD_PID).peak_rss_bytes, 3_000);
}

#[test]
fn retains_cumulative_baselines_while_a_process_is_absent_from_an_intermediate_sample() {
    let first = snapshot(1, started(), 100, 1_000);
    let mut absent = snapshot(2, started() + 1_000, 0, 0);
    absent.processes.retain(|p| p.pid != CHILD_PID);
    let returned = snapshot(3, started() + 2_000, 350, 5_000);
    let h = history(started() + 3_000, 10_000, None, None, &[first, absent, returned]);
    let child = top(&h, CHILD_PID);
    assert_eq!(child.cpu_time_ms, 250);
    assert_eq!(child.io_write_bytes, 4_000);
}

#[test]
fn uses_an_exact_current_electron_identity_for_slightly_older_native_samples() {
    let desktop = desktop();
    let h = history(started() + 2_000, 10_000, None, Some(&desktop), &[snapshot(1, started(), 100, 1_000)]);
    assert_eq!(top(&h, ELECTRON_PID).category, C::ElectronMain);
}

#[test]
fn uses_the_preceding_sample_as_a_baseline_without_attributing_pre_window_deltas() {
    let read_at = started() + 10_000;
    let h = history(
        read_at,
        5_000,
        None,
        None,
        &[
            snapshot(1, started(), 100, 1_000),
            snapshot(2, started() + 5_000, 600, 6_000),
            snapshot(3, read_at + 1_000, 10_000, 20_000),
        ],
    );
    let child = top(&h, CHILD_PID);
    assert_eq!(child.sample_count, 1);
    assert_eq!(child.cpu_time_ms, 0);
    assert_eq!(child.io_write_bytes, 0);
    assert_eq!(h.buckets.iter().map(|b| b.io_write_bytes).sum::<i64>(), 0);
    assert!(h.buckets.iter().all(|b| b.started_at.as_millis() <= read_at));
}

#[test]
fn prorates_a_cumulative_delta_that_crosses_the_history_window_boundary() {
    let read_at = started() + 10_000;
    let h = history(
        read_at,
        5_000,
        None,
        None,
        &[snapshot(1, started(), 100, 1_000), snapshot(2, started() + 7_500, 850, 8_500)],
    );
    let child = top(&h, CHILD_PID);
    assert_eq!(child.cpu_time_ms, 250);
    assert_eq!(child.io_write_bytes, 2_500);
    assert_eq!(h.buckets.iter().map(|b| b.io_write_bytes).sum::<i64>(), 2_500);
}

#[test]
fn replays_the_electron_root_identity_recorded_with_each_native_sample() {
    let mut old_electron = snapshot(1, started(), 100, 1_000);
    old_electron.external_processes = Some(vec![ResourceMonitorExternalProcess {
        pid: ELECTRON_PID,
        start_time_ms: Some(20),
    }]);
    let mut restarted = snapshot(2, started() + 1_000, 200, 2_000);
    restarted.external_processes = Some(vec![ResourceMonitorExternalProcess {
        pid: 201,
        start_time_ms: Some(40),
    }]);
    restarted.processes.retain(|p| p.pid != ELECTRON_PID);
    restarted.processes.push(with(process_sample(ELECTRON_PID, SERVER_PID, 999), |p| {
        p.name = "reused".into();
        p.command = "unrelated process".into();
    }));
    restarted.processes.push(with(process_sample(201, 1, 40), |p| {
        p.name = "electron".into();
        p.command = "electron".into();
    }));
    let h = history(started() + 2_000, 10_000, None, None, &[old_electron, restarted]);
    let by_identity = |pid: i64, start: i64| {
        h.top_processes
            .iter()
            .find(|p| p.identity.pid == pid && p.identity.start_time_ms == start)
            .unwrap()
            .category
    };
    assert_eq!(by_identity(ELECTRON_PID, 20), C::ElectronMain);
    assert_eq!(by_identity(ELECTRON_PID, 999), C::ServerChild);
    assert_eq!(top(&h, 201).category, C::ElectronMain);
}
