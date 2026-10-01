//! `resourceTelemetry/ResourceTelemetryHistory.ts`: replays the monitor's retained native
//! snapshots through [`merge_processes`] into time buckets and per-process summaries.

use std::collections::HashMap;

use zc_contracts::{
    DateTimeUtc, DesktopHostTelemetrySnapshot, JsNumber, ResourceMonitorSnapshotEvent, ResourceTelemetryHealth, ResourceTelemetryHistory,
    ResourceTelemetryHistoryBucket, ResourceTelemetryProcess, ResourceTelemetryProcessSummary,
};

use crate::model::{is_backend_category, merge_processes, process_identity_key, MergeProcessesInput, ProcessState, TelemetryCounters};

const MAX_HISTORY_WINDOW_MS: i64 = 60 * 60_000;

/// `normalizeResourceTelemetryHistoryInput`: window in [1 s, 1 h], bucket in [1 s, window].
pub fn normalize_history_input(window_ms: i64, bucket_ms: i64) -> (i64, i64) {
    let window_ms = window_ms.clamp(1_000, MAX_HISTORY_WINDOW_MS);
    (window_ms, bucket_ms.min(window_ms).max(1_000))
}

#[derive(Debug, Clone, Copy)]
struct AggregateSample {
    sampled_at_ms: i64,
    cpu_percent: f64,
    rss_bytes: i64,
    process_count: i64,
    io_read_bytes: i64,
    io_write_bytes: i64,
}

struct ProcessSample {
    sampled_at_ms: i64,
    process: ResourceTelemetryProcess,
    cpu_time_ms: i64,
    io_read_bytes: i64,
    io_write_bytes: i64,
}

pub struct BuildHistoryInput<'a> {
    pub read_at_ms: i64,
    pub window_ms: i64,
    pub bucket_ms: i64,
    pub sample_interval_ms: i64,
    pub server_pid: i64,
    pub sidecar_pid: Option<i64>,
    pub desktop_snapshot: Option<&'a DesktopHostTelemetrySnapshot>,
    pub snapshots: &'a [ResourceMonitorSnapshotEvent],
    pub health: ResourceTelemetryHealth,
}

/// The wire history plus the backend-only buckets `server.getProcessResourceHistory` shows.
#[derive(Debug, Clone)]
pub struct HistoryWithLegacyBuckets {
    pub history: ResourceTelemetryHistory,
    pub legacy_backend_buckets: Vec<ResourceTelemetryHistoryBucket>,
}

fn date(ms: i64) -> DateTimeUtc {
    DateTimeUtc::from_millis(ms).unwrap_or_else(|_| DateTimeUtc::now())
}

fn summarize_processes(samples: Vec<ProcessSample>) -> Vec<ResourceTelemetryProcessSummary> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<ProcessSample>> = HashMap::new();
    for sample in samples {
        let key = process_identity_key(sample.process.identity.pid, sample.process.identity.start_time_ms);
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(sample);
    }
    let mut summaries: Vec<ResourceTelemetryProcessSummary> = order
        .into_iter()
        .filter_map(|key| groups.remove(&key))
        .map(|mut sorted| {
            sorted.sort_by_key(|sample| sample.sampled_at_ms);
            let first = &sorted[0];
            let latest = &sorted[sorted.len() - 1];
            let cpu_total: f64 = sorted.iter().map(|s| s.process.cpu_percent.0).sum();
            ResourceTelemetryProcessSummary {
                identity: latest.process.identity.clone(),
                ppid: latest.process.ppid,
                depth: latest.process.depth,
                name: latest.process.name.clone(),
                command: latest.process.command.clone(),
                category: latest.process.category,
                first_seen_at: first.process.first_seen_at,
                last_seen_at: latest.process.last_seen_at,
                current_cpu_percent: latest.process.cpu_percent,
                avg_cpu_percent: JsNumber(cpu_total / sorted.len() as f64),
                max_cpu_percent: JsNumber(sorted.iter().map(|s| s.process.cpu_percent.0).fold(f64::NEG_INFINITY, f64::max)),
                cpu_time_ms: sorted.iter().map(|s| s.cpu_time_ms).sum(),
                current_rss_bytes: latest.process.resident_bytes,
                peak_rss_bytes: sorted.iter().map(|s| s.process.resident_bytes).max().unwrap_or(0),
                io_read_bytes: sorted.iter().map(|s| s.io_read_bytes).sum(),
                io_write_bytes: sorted.iter().map(|s| s.io_write_bytes).sum(),
                io_semantics: latest.process.io_semantics,
                sample_count: sorted.len() as i64,
            }
        })
        .collect();
    summaries.sort_by(|a, b| b.cpu_time_ms.cmp(&a.cpu_time_ms).then(b.peak_rss_bytes.cmp(&a.peak_rss_bytes)));
    summaries
}

fn build_buckets(samples: &[AggregateSample], now_ms: i64, window_ms: i64, bucket_ms: i64) -> Vec<ResourceTelemetryHistoryBucket> {
    let mut buckets = Vec::new();
    let mut started_at_ms = now_ms - window_ms;
    while started_at_ms < now_ms {
        let ended_at_ms = now_ms.min(started_at_ms + bucket_ms);
        let inside: Vec<&AggregateSample> = samples
            .iter()
            .filter(|s| {
                s.sampled_at_ms >= started_at_ms
                    && if ended_at_ms == now_ms {
                        s.sampled_at_ms <= ended_at_ms
                    } else {
                        s.sampled_at_ms < ended_at_ms
                    }
            })
            .collect();
        let count = inside.len();
        let cpu_total: f64 = inside.iter().map(|s| s.cpu_percent).sum();
        buckets.push(ResourceTelemetryHistoryBucket {
            started_at: date(started_at_ms),
            ended_at: date(ended_at_ms),
            avg_cpu_percent: JsNumber(if count == 0 { 0.0 } else { cpu_total / count as f64 }),
            max_cpu_percent: JsNumber(if count == 0 {
                0.0
            } else {
                inside.iter().map(|s| s.cpu_percent).fold(f64::NEG_INFINITY, f64::max)
            }),
            max_rss_bytes: inside.iter().map(|s| s.rss_bytes).max().unwrap_or(0),
            io_read_bytes: inside.iter().map(|s| s.io_read_bytes).sum(),
            io_write_bytes: inside.iter().map(|s| s.io_write_bytes).sum(),
            max_process_count: inside.iter().map(|s| s.process_count).max().unwrap_or(0),
        });
        started_at_ms += bucket_ms;
    }
    buckets
}

/// JavaScript `Math.round` (halves toward +∞).
fn js_round(value: f64) -> i64 {
    (value + 0.5).floor() as i64
}

/// `buildResourceTelemetryHistory`.
pub fn build_history(input: BuildHistoryInput<'_>) -> HistoryWithLegacyBuckets {
    let read_at_ms = input.read_at_ms;
    let (window_ms, bucket_ms) = normalize_history_input(input.window_ms, input.bucket_ms);
    let window_start_ms = read_at_ms - window_ms;
    let mut eligible: Vec<&ResourceMonitorSnapshotEvent> = input.snapshots.iter().filter(|s| s.sampled_at_unix_ms <= read_at_ms).collect();
    eligible.sort_by_key(|s| s.sampled_at_unix_ms);
    let preceding = eligible.iter().rev().find(|s| s.sampled_at_unix_ms < window_start_ms).copied();
    let mut snapshots: Vec<&ResourceMonitorSnapshotEvent> = preceding.into_iter().collect();
    snapshots.extend(eligible.iter().filter(|s| s.sampled_at_unix_ms >= window_start_ms));

    let mut aggregate_samples = Vec::new();
    let mut legacy_samples = Vec::new();
    let mut process_samples = Vec::new();
    let mut previous: HashMap<String, ProcessState> = HashMap::new();
    let mut counters = TelemetryCounters::default();
    let mut previous_snapshot_at_ms: Option<i64> = None;

    for snapshot in snapshots {
        let fraction = match previous_snapshot_at_ms {
            Some(prev) if prev < window_start_ms && snapshot.sampled_at_unix_ms > prev => {
                ((snapshot.sampled_at_unix_ms - window_start_ms) as f64 / (snapshot.sampled_at_unix_ms - prev) as f64).clamp(0.0, 1.0)
            }
            _ => 1.0,
        };
        previous_snapshot_at_ms = Some(snapshot.sampled_at_unix_ms);
        let recorded: Vec<(i64, Option<i64>)> = match &snapshot.external_processes {
            Some(external) => external.iter().map(|p| (p.pid, p.start_time_ms)).collect(),
            None => match input.desktop_snapshot {
                None => Vec::new(),
                Some(desktop) => vec![(
                    desktop.electron_pid,
                    desktop
                        .electron_processes
                        .iter()
                        .find(|m| m.pid == desktop.electron_pid)
                        .map(|m| m.creation_time_ms),
                )],
            },
        };
        let root_pids: Vec<i64> = recorded.iter().map(|(pid, _)| *pid).collect();
        let root_start_times: HashMap<i64, i64> = recorded.iter().filter_map(|(pid, start)| start.map(|s| (*pid, s))).collect();
        let merged = merge_processes(MergeProcessesInput {
            server_pid: input.server_pid,
            sidecar_pid: input.sidecar_pid,
            fallback_sampled_at_ms: snapshot.sampled_at_unix_ms,
            native_snapshot: Some(snapshot),
            desktop_snapshot: None,
            electron_root_pids: Some(&root_pids),
            electron_root_start_times: Some(&root_start_times),
            previous: &previous,
            counters,
            update_previous: true,
        });
        previous.extend(merged.previous);
        counters = merged.counters;
        if snapshot.sampled_at_unix_ms < window_start_ms {
            continue;
        }
        let deltas: Vec<_> = if fraction == 1.0 {
            merged.deltas
        } else {
            merged
                .deltas
                .into_iter()
                .map(|mut d| {
                    d.cpu_time_ms = js_round(d.cpu_time_ms as f64 * fraction);
                    d.io_read_bytes = js_round(d.io_read_bytes as f64 * fraction);
                    d.io_write_bytes = js_round(d.io_write_bytes as f64 * fraction);
                    d
                })
                .collect()
        };
        aggregate_samples.push(AggregateSample {
            sampled_at_ms: snapshot.sampled_at_unix_ms,
            cpu_percent: merged.groups.all_t3.current_cpu_percent.0,
            rss_bytes: merged.groups.all_t3.current_rss_bytes,
            process_count: merged.groups.all_t3.process_count,
            io_read_bytes: deltas.iter().map(|d| d.io_read_bytes).sum(),
            io_write_bytes: deltas.iter().map(|d| d.io_write_bytes).sum(),
        });
        let backend = deltas.iter().filter(|d| is_backend_category(d.category));
        legacy_samples.push(AggregateSample {
            sampled_at_ms: snapshot.sampled_at_unix_ms,
            cpu_percent: merged.groups.backend.current_cpu_percent.0,
            rss_bytes: merged.groups.backend.current_rss_bytes,
            process_count: merged.groups.backend.process_count,
            io_read_bytes: backend.clone().map(|d| d.io_read_bytes).sum(),
            io_write_bytes: backend.map(|d| d.io_write_bytes).sum(),
        });
        let by_identity: HashMap<&str, &crate::model::ProcessDelta> = deltas.iter().map(|d| (d.identity_key.as_str(), d)).collect();
        for process in merged.processes {
            let delta = by_identity
                .get(process_identity_key(process.identity.pid, process.identity.start_time_ms).as_str())
                .copied();
            process_samples.push(ProcessSample {
                sampled_at_ms: snapshot.sampled_at_unix_ms,
                cpu_time_ms: delta.map(|d| d.cpu_time_ms).unwrap_or(0),
                io_read_bytes: delta.map(|d| d.io_read_bytes).unwrap_or(0),
                io_write_bytes: delta.map(|d| d.io_write_bytes).unwrap_or(0),
                process,
            });
        }
    }

    let retained = (aggregate_samples.len() + process_samples.len()) as i64;
    HistoryWithLegacyBuckets {
        legacy_backend_buckets: build_buckets(&legacy_samples, read_at_ms, window_ms, bucket_ms),
        history: ResourceTelemetryHistory {
            read_at: date(read_at_ms),
            window_ms,
            bucket_ms,
            sample_interval_ms: input.sample_interval_ms,
            retained_sample_count: retained,
            buckets: build_buckets(&aggregate_samples, read_at_ms, window_ms, bucket_ms),
            top_processes: summarize_processes(process_samples),
            health: input.health,
        },
    }
}
