//! `resourceTelemetry/Model.test.ts`.

mod common;

use std::collections::HashMap;

use common::*;
use zc_contracts::{
    DesktopElectronProcessType as E, DesktopHostTelemetrySnapshot, JsNumber, ResourceMonitorProcessSampleIoSemantics, ResourceMonitorSnapshotEvent,
    ResourceTelemetryIoSemantics, ResourceTelemetryProcessCategory as C,
};
use zc_telemetry::model::{merge_processes, MergeProcessesInput, MergeProcessesResult, TelemetryCounters};

const SERVER_PID: i64 = 100;

fn base() -> i64 {
    ms("2026-06-17T12:00:00.000Z")
}

fn merge(
    native: &ResourceMonitorSnapshotEvent,
    desktop: Option<&DesktopHostTelemetrySnapshot>,
    previous: Option<&MergeProcessesResult>,
    sidecar_pid: Option<i64>,
) -> MergeProcessesResult {
    let empty = HashMap::new();
    merge_processes(MergeProcessesInput {
        server_pid: SERVER_PID,
        sidecar_pid,
        fallback_sampled_at_ms: native.sampled_at_unix_ms,
        native_snapshot: Some(native),
        desktop_snapshot: desktop,
        electron_root_pids: None,
        electron_root_start_times: None,
        previous: previous.map_or(&empty, |p| &p.previous),
        counters: previous.map_or(TelemetryCounters::default(), |p| p.counters),
        update_previous: true,
    })
}

fn find(result: &MergeProcessesResult, pid: i64) -> &zc_contracts::ResourceTelemetryProcess {
    result.processes.iter().find(|p| p.identity.pid == pid).unwrap()
}

fn server(cpu: i64, read: i64, write: i64) -> zc_contracts::ResourceMonitorProcessSample {
    with(process_sample(SERVER_PID, 1, 1_000), |p| {
        p.cpu_time_ms = cpu;
        p.io_read_bytes = read;
        p.io_write_bytes = write;
    })
}

#[test]
fn builds_complete_descendant_depths_and_isolates_monitor_overhead() {
    let result = merge(
        &native_snapshot(
            base(),
            vec![
                process_sample(SERVER_PID, 1, 1_000),
                process_sample(200, SERVER_PID, 2_000),
                process_sample(201, 200, 3_000),
                process_sample(202, 201, 4_000),
                process_sample(900, SERVER_PID, 5_000),
            ],
            1,
        ),
        None,
        None,
        Some(900),
    );
    assert_eq!(
        result.processes.iter().map(|p| (p.identity.pid, p.depth)).collect::<Vec<_>>(),
        [(100, 0), (200, 1), (201, 2), (202, 3), (900, 1)]
    );
    assert_eq!(find(&result, 900).category, C::ResourceMonitor);
    assert_eq!(result.groups.backend.process_count, 4);
    assert_eq!(result.groups.monitor.process_count, 1);
    assert_eq!(result.groups.monitor.process_starts, 1);
    assert_eq!(result.groups.all_t3.process_starts, 5);
}

#[test]
fn deduplicates_electron_metrics_and_classifies_electron_descendants() {
    let electron_start = 10_000;
    let mut browser = electron_metric(300, electron_start + 500, E::Browser);
    browser.name = Some("electron".into());
    let mut utility = electron_metric(301, electron_start + 500, E::Utility);
    utility.name = Some("network-service".into());
    let result = merge(
        &native_snapshot(
            base(),
            vec![
                process_sample(SERVER_PID, 1, 1_000),
                process_sample(300, 1, electron_start),
                process_sample(301, 300, electron_start + 1),
            ],
            1,
        ),
        Some(&desktop_snapshot(base(), vec![browser, utility])),
        None,
        None,
    );
    assert_eq!(result.processes.iter().filter(|p| p.identity.pid == 300).count(), 1);
    assert_eq!(find(&result, 300).category, C::ElectronMain);
    assert_eq!(find(&result, 301).category, C::ElectronUtility);
    assert_eq!(find(&result, 301).depth, 1);
    assert_eq!(result.groups.electron.process_count, 2);
}

#[test]
fn ignores_stale_electron_metrics_after_pid_reuse() {
    let result = merge(
        &native_snapshot(base(), vec![process_sample(SERVER_PID, 1, 1_000), process_sample(300, SERVER_PID, 50_000)], 1),
        Some(&desktop_snapshot(base(), vec![electron_metric(300, 10_000, E::Browser)])),
        None,
        None,
    );
    assert_eq!(find(&result, 300).category, C::ServerChild);
    assert_eq!(result.groups.electron.process_count, 0);
}

#[test]
fn derives_cumulative_cpu_time_for_synthetic_electron_only_processes() {
    let mut metric = electron_metric(300, 10_000, E::Browser);
    metric.cpu_percent = JsNumber(50.0);
    let first = merge(
        &native_snapshot(base(), vec![process_sample(SERVER_PID, 1, 1_000)], 1),
        Some(&desktop_snapshot(base(), vec![metric.clone()])),
        None,
        None,
    );
    let second = merge(
        &native_snapshot(base() + 1_000, vec![process_sample(SERVER_PID, 1, 1_000)], 2),
        Some(&desktop_snapshot(base() + 1_000, vec![metric])),
        Some(&first),
        None,
    );
    assert_eq!(find(&second, 300).cpu_time_ms, 500);
    assert_eq!(second.groups.electron.cpu_time_ms, 500);
}

#[test]
fn uses_the_native_timestamp_for_native_cumulative_counter_deltas() {
    let first = merge(
        &native_snapshot(base(), vec![server(1_000, 0, 0)], 1),
        Some(&desktop_snapshot(base() + 10_000, vec![])),
        None,
        None,
    );
    let second = merge(
        &native_snapshot(base() + 1_000, vec![server(1_500, 0, 0)], 2),
        Some(&desktop_snapshot(base() + 11_000, vec![])),
        Some(&first),
        None,
    );
    assert_eq!(second.processes[0].cpu_percent.0, 50.0);
    assert_eq!(second.groups.backend.cpu_time_ms, 500);
}

#[test]
fn does_not_advance_synthetic_cpu_time_when_reusing_the_same_desktop_sample() {
    let mut metric = electron_metric(300, 10_000, E::Browser);
    metric.cpu_percent = JsNumber(50.0);
    let desktop = desktop_snapshot(base(), vec![metric]);
    let first = merge(
        &native_snapshot(base(), vec![process_sample(SERVER_PID, 1, 1_000)], 1),
        Some(&desktop),
        None,
        None,
    );
    let second = merge(
        &native_snapshot(base() + 1_000, vec![process_sample(SERVER_PID, 1, 1_000)], 2),
        Some(&desktop),
        Some(&first),
        None,
    );
    assert_eq!(find(&second, 300).cpu_time_ms, 0);
    assert_eq!(second.groups.electron.cpu_time_ms, 0);
}

#[test]
fn does_not_apply_an_explicit_electron_root_to_a_reused_pid() {
    let empty = HashMap::new();
    let native = native_snapshot(base(), vec![process_sample(SERVER_PID, 1, 1_000), process_sample(300, 1, 10_000)], 1);
    let desktop = desktop_snapshot(base(), vec![electron_metric(300, 10_000, E::Browser)]);
    let first = merge_processes(MergeProcessesInput {
        server_pid: SERVER_PID,
        sidecar_pid: None,
        fallback_sampled_at_ms: base(),
        native_snapshot: Some(&native),
        desktop_snapshot: Some(&desktop),
        electron_root_pids: Some(&[300]),
        electron_root_start_times: None,
        previous: &empty,
        counters: TelemetryCounters::default(),
        update_previous: true,
    });
    let reused_native = native_snapshot(
        base() + 1_000,
        vec![process_sample(SERVER_PID, 1, 1_000), process_sample(300, SERVER_PID, 20_000)],
        2,
    );
    let reused = merge_processes(MergeProcessesInput {
        server_pid: SERVER_PID,
        sidecar_pid: None,
        fallback_sampled_at_ms: base() + 1_000,
        native_snapshot: Some(&reused_native),
        desktop_snapshot: None,
        electron_root_pids: Some(&[300]),
        electron_root_start_times: None,
        previous: &first.previous,
        counters: first.counters,
        update_previous: true,
    });
    assert_eq!(find(&reused, 300).category, C::ServerChild);
    assert_eq!(reused.groups.electron.process_count, 0);
}

#[test]
fn derives_rates_from_cumulative_counters_and_preserves_io_semantics() {
    let all_io = |cpu, read, write| with(server(cpu, read, write), |p| p.io_semantics = ResourceMonitorProcessSampleIoSemantics::AllIo);
    let first = merge(&native_snapshot(base(), vec![all_io(1_000, 10_000, 20_000)], 1), None, None, None);
    let second = merge(
        &native_snapshot(base() + 1_000, vec![all_io(1_250, 12_000, 23_000)], 2),
        None,
        Some(&first),
        None,
    );
    let server = &second.processes[0];
    assert_eq!(server.cpu_percent.0, 25.0);
    assert_eq!(server.io_read_bytes_per_second.0, 2_000.0);
    assert_eq!(server.io_write_bytes_per_second.0, 3_000.0);
    assert_eq!(server.io_semantics, ResourceTelemetryIoSemantics::AllIo);
    assert_eq!(second.groups.backend.cpu_time_ms, 250);
    assert_eq!(second.groups.backend.io_read_bytes, 2_000);
    assert_eq!(second.groups.backend.io_write_bytes, 3_000);
}

#[test]
fn derives_deltas_at_the_constrained_15_second_sampling_cadence() {
    let first = merge(&native_snapshot(base(), vec![server(1_000, 10_000, 20_000)], 1), None, None, None);
    let second = merge(
        &native_snapshot(base() + 15_000, vec![server(2_500, 25_000, 50_000)], 2),
        None,
        Some(&first),
        None,
    );
    assert_eq!(second.processes[0].cpu_percent.0, 10.0);
    assert_eq!(second.processes[0].io_read_bytes_per_second.0, 1_000.0);
    assert_eq!(second.processes[0].io_write_bytes_per_second.0, 2_000.0);
    assert_eq!(second.groups.backend.cpu_time_ms, 1_500);
    assert_eq!(second.groups.backend.io_read_bytes, 15_000);
    assert_eq!(second.groups.backend.io_write_bytes, 30_000);
}

#[test]
fn preserves_native_rates_while_applying_a_desktop_only_update() {
    let first = merge(&native_snapshot(base(), vec![server(1_000, 10_000, 20_000)], 1), None, None, None);
    let native = native_snapshot(base() + 1_000, vec![server(1_250, 12_000, 23_000)], 2);
    let second = merge(&native, None, Some(&first), None);
    let desktop = desktop_snapshot(base() + 1_500, vec![]);
    let desktop_only = merge_processes(MergeProcessesInput {
        server_pid: SERVER_PID,
        sidecar_pid: None,
        fallback_sampled_at_ms: base() + 1_000,
        native_snapshot: Some(&native),
        desktop_snapshot: Some(&desktop),
        electron_root_pids: None,
        electron_root_start_times: None,
        previous: &second.previous,
        counters: second.counters,
        update_previous: false,
    });
    assert_eq!(desktop_only.processes[0].cpu_percent.0, 25.0);
    assert_eq!(desktop_only.processes[0].io_read_bytes_per_second.0, 2_000.0);
    assert_eq!(desktop_only.processes[0].io_write_bytes_per_second.0, 3_000.0);
    assert_eq!(desktop_only.sampled_at_ms, base() + 1_500);
}

#[test]
fn resets_deltas_when_counters_decrease_or_the_sampling_gap_is_unsafe() {
    let first = merge(&native_snapshot(base(), vec![server(1_000, 10_000, 20_000)], 1), None, None, None);
    let decreased = merge(&native_snapshot(base() + 1_000, vec![server(100, 100, 200)], 2), None, Some(&first), None);
    let delayed = merge(
        &native_snapshot(base() + 90_000, vec![server(10_000, 100_000, 200_000)], 3),
        None,
        Some(&decreased),
        None,
    );
    for result in [&decreased, &delayed] {
        assert_eq!(result.processes[0].cpu_percent.0, 0.0);
        assert_eq!(result.processes[0].io_read_bytes_per_second.0, 0.0);
        assert_eq!(result.processes[0].io_write_bytes_per_second.0, 0.0);
    }
    assert_eq!(delayed.groups.backend.cpu_time_ms, 0);
    assert_eq!(delayed.groups.backend.io_read_bytes, 0);
    assert_eq!(delayed.groups.backend.io_write_bytes, 0);
}

#[test]
fn treats_reused_pids_as_an_exit_plus_a_new_process() {
    let first = merge(
        &native_snapshot(base(), vec![process_sample(SERVER_PID, 1, 1_000), process_sample(200, SERVER_PID, 2_000)], 1),
        None,
        None,
        None,
    );
    let second = merge(
        &native_snapshot(
            base() + 1_000,
            vec![
                process_sample(SERVER_PID, 1, 1_000),
                with(process_sample(200, SERVER_PID, 9_000), |p| {
                    p.cpu_time_ms = 999;
                    p.io_read_bytes = 999;
                    p.io_write_bytes = 999;
                }),
            ],
            2,
        ),
        None,
        Some(&first),
        None,
    );
    let reused = find(&second, 200);
    assert_eq!(reused.identity.start_time_ms, 9_000);
    assert_eq!(reused.cpu_percent.0, 0.0);
    assert_eq!(reused.io_read_bytes_per_second.0, 0.0);
    assert_eq!(second.groups.backend.process_starts, 3);
    assert_eq!(second.groups.backend.process_exits, 1);
}
