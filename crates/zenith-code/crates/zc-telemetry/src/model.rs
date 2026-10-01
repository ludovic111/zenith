//! `resourceTelemetry/Model.ts`: merges a native process-tree sample with the desktop
//! (Electron) metrics into the `ResourceTelemetryProcess` rows, their tree order and depth,
//! categories, rates derived from cumulative counters, and the group aggregates.

use std::collections::{HashMap, HashSet, VecDeque};

use zc_contracts::{
    DateTimeUtc, DesktopElectronProcessMetric, DesktopElectronProcessType, DesktopHostTelemetrySnapshot, JsNumber, ResourceMonitorProcessSample,
    ResourceMonitorProcessSampleIoSemantics, ResourceMonitorSnapshotEvent, ResourceTelemetryAggregate, ResourceTelemetryGroups, ResourceTelemetryIoSemantics,
    ResourceTelemetryProcess, ResourceTelemetryProcessCategory, ResourceTelemetryProcessIdentity,
};

const MAX_DELTA_INTERVAL_MS: i64 = 30_000;
const ELECTRON_IDENTITY_TOLERANCE_MS: i64 = 2_000;

/// A process as last seen, with the time its counters were sampled.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessState {
    pub process: ResourceTelemetryProcess,
    pub sampled_at_ms: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GroupCounters {
    pub cpu_time_ms: i64,
    pub io_read_bytes: i64,
    pub io_write_bytes: i64,
    pub process_starts: i64,
    pub process_exits: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TelemetryCounters {
    pub backend: GroupCounters,
    pub electron: GroupCounters,
    pub monitor: GroupCounters,
    pub all_t3: GroupCounters,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProcessDelta {
    pub identity_key: String,
    pub category: ResourceTelemetryProcessCategory,
    pub cpu_time_ms: i64,
    pub io_read_bytes: i64,
    pub io_write_bytes: i64,
}

pub struct MergeProcessesInput<'a> {
    pub server_pid: i64,
    pub sidecar_pid: Option<i64>,
    pub fallback_sampled_at_ms: i64,
    pub native_snapshot: Option<&'a ResourceMonitorSnapshotEvent>,
    pub desktop_snapshot: Option<&'a DesktopHostTelemetrySnapshot>,
    /// In the order they were recorded (duplicates ignored).
    pub electron_root_pids: Option<&'a [i64]>,
    pub electron_root_start_times: Option<&'a HashMap<i64, i64>>,
    pub previous: &'a HashMap<String, ProcessState>,
    pub counters: TelemetryCounters,
    pub update_previous: bool,
}

#[derive(Debug, Clone)]
pub struct MergeProcessesResult {
    pub sampled_at_ms: i64,
    pub processes: Vec<ResourceTelemetryProcess>,
    pub previous: HashMap<String, ProcessState>,
    pub counters: TelemetryCounters,
    pub groups: ResourceTelemetryGroups,
    pub deltas: Vec<ProcessDelta>,
}

pub fn process_identity_key(pid: i64, start_time_ms: i64) -> String {
    format!("{pid}:{start_time_ms}")
}

fn finite_non_negative(value: f64) -> f64 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Backend,
    Electron,
    Monitor,
}

fn category_group(category: ResourceTelemetryProcessCategory) -> Group {
    use ResourceTelemetryProcessCategory as C;
    match category {
        C::ResourceMonitor => Group::Monitor,
        C::ElectronMain | C::ElectronRenderer | C::ElectronGpu | C::ElectronUtility => Group::Electron,
        _ => Group::Backend,
    }
}

/// Every category the TS server counts as its own backend (`isLegacyBackendCategory`).
pub fn is_backend_category(category: ResourceTelemetryProcessCategory) -> bool {
    use ResourceTelemetryProcessCategory as C;
    matches!(category, C::Server | C::ServerChild | C::ProviderRoot | C::TerminalRoot)
}

fn electron_category(metric: &DesktopElectronProcessMetric) -> ResourceTelemetryProcessCategory {
    match metric.r#type {
        DesktopElectronProcessType::Browser => ResourceTelemetryProcessCategory::ElectronMain,
        DesktopElectronProcessType::Tab => ResourceTelemetryProcessCategory::ElectronRenderer,
        DesktopElectronProcessType::GPU => ResourceTelemetryProcessCategory::ElectronGpu,
        _ => ResourceTelemetryProcessCategory::ElectronUtility,
    }
}

fn inferred_electron_category(process: &ResourceMonitorProcessSample) -> ResourceTelemetryProcessCategory {
    let command = process.command.to_lowercase();
    if command.contains("--type=renderer") {
        ResourceTelemetryProcessCategory::ElectronRenderer
    } else if command.contains("--type=gpu-process") {
        ResourceTelemetryProcessCategory::ElectronGpu
    } else {
        ResourceTelemetryProcessCategory::ElectronUtility
    }
}

fn match_electron_metric<'a>(
    process: &ResourceMonitorProcessSample,
    metrics_by_pid: &HashMap<i64, &'a DesktopElectronProcessMetric>,
) -> Option<&'a DesktopElectronProcessMetric> {
    let metric = metrics_by_pid.get(&process.pid)?;
    ((metric.creation_time_ms - process.start_time_ms).abs() <= ELECTRON_IDENTITY_TOLERANCE_MS).then_some(*metric)
}

fn synthetic_native_sample(metric: &DesktopElectronProcessMetric, sampled_at_ms: i64, previous: Option<&ProcessState>) -> ResourceMonitorProcessSample {
    let cpu_time_ms = match (metric.cumulative_cpu_seconds, previous) {
        (Some(seconds), _) => (seconds.0 * 1_000.0).round().max(0.0) as i64,
        (None, Some(previous)) => {
            let elapsed = (sampled_at_ms - previous.sampled_at_ms) as f64;
            (previous.process.cpu_time_ms as f64 + (elapsed * metric.cpu_percent.0 / 100.0).max(0.0)).round() as i64
        }
        (None, None) => 0,
    };
    let label = metric
        .name
        .clone()
        .or_else(|| metric.service_name.clone())
        .unwrap_or_else(|| metric.r#type.as_str().to_owned());
    ResourceMonitorProcessSample {
        pid: metric.pid,
        ppid: 0,
        start_time_ms: metric.creation_time_ms,
        run_time_ms: (sampled_at_ms - metric.creation_time_ms).max(0),
        name: label.clone(),
        command: label,
        status: "Running".into(),
        cpu_percent: metric.cpu_percent,
        cpu_time_ms,
        resident_bytes: metric.working_set_bytes,
        virtual_bytes: 0,
        io_read_bytes: 0,
        io_write_bytes: 0,
        io_semantics: ResourceMonitorProcessSampleIoSemantics::Storage,
    }
}

fn process_depths(processes: &[ResourceMonitorProcessSample], roots: &[i64]) -> HashMap<i64, i64> {
    let mut children_by_parent: HashMap<i64, Vec<i64>> = HashMap::new();
    for process in processes {
        children_by_parent.entry(process.ppid).or_default().push(process.pid);
    }
    let mut depths = HashMap::new();
    let mut queue: VecDeque<(i64, i64)> = roots.iter().map(|pid| (*pid, 0)).collect();
    while let Some((pid, depth)) = queue.pop_front() {
        if depths.contains_key(&pid) {
            continue;
        }
        depths.insert(pid, depth);
        for child in children_by_parent.get(&pid).into_iter().flatten() {
            queue.push_back((*child, depth + 1));
        }
    }
    depths
}

fn is_electron_descendant(pid: i64, by_pid: &HashMap<i64, &ResourceMonitorProcessSample>, electron_pids: &HashSet<i64>) -> bool {
    let mut visited = HashSet::new();
    let mut current_pid = pid;
    while visited.insert(current_pid) {
        if electron_pids.contains(&current_pid) {
            return true;
        }
        let Some(current) = by_pid.get(&current_pid) else {
            return false;
        };
        if current.ppid <= 0 || current.ppid == current_pid {
            return false;
        }
        current_pid = current.ppid;
    }
    false
}

fn has_electron_ancestor(process: &ResourceMonitorProcessSample, by_pid: &HashMap<i64, &ResourceMonitorProcessSample>, electron_pids: &HashSet<i64>) -> bool {
    let mut visited = HashSet::new();
    let mut current_pid = process.ppid;
    while current_pid > 0 && visited.insert(current_pid) {
        if electron_pids.contains(&current_pid) {
            return true;
        }
        let Some(current) = by_pid.get(&current_pid) else {
            return false;
        };
        if current.ppid == current_pid {
            return false;
        }
        current_pid = current.ppid;
    }
    false
}

/// Depth-first from the roots (children by pid), then whatever is left by depth and pid.
fn order_process_tree(processes: Vec<ResourceTelemetryProcess>, root_pids: &[i64]) -> Vec<ResourceTelemetryProcess> {
    let mut children_by_parent: HashMap<i64, Vec<usize>> = HashMap::new();
    let mut index_by_pid: HashMap<i64, usize> = HashMap::new();
    for (index, process) in processes.iter().enumerate() {
        children_by_parent.entry(process.ppid).or_default().push(index);
        // A later row with the same pid wins, like the TS `new Map(...)`.
        index_by_pid.insert(process.identity.pid, index);
    }
    for children in children_by_parent.values_mut() {
        children.sort_by_key(|index| processes[*index].identity.pid);
    }

    let mut ordered = Vec::with_capacity(processes.len());
    let mut visited = HashSet::new();
    fn visit(
        index: usize,
        processes: &[ResourceTelemetryProcess],
        children_by_parent: &HashMap<i64, Vec<usize>>,
        visited: &mut HashSet<i64>,
        ordered: &mut Vec<usize>,
    ) {
        let pid = processes[index].identity.pid;
        if !visited.insert(pid) {
            return;
        }
        ordered.push(index);
        for child in children_by_parent.get(&pid).into_iter().flatten() {
            visit(*child, processes, children_by_parent, visited, ordered);
        }
    }
    for root in root_pids {
        if let Some(index) = index_by_pid.get(root) {
            visit(*index, &processes, &children_by_parent, &mut visited, &mut ordered);
        }
    }
    let mut rest: Vec<usize> = (0..processes.len()).collect();
    rest.sort_by(|a, b| {
        let (a, b) = (&processes[*a], &processes[*b]);
        a.depth.cmp(&b.depth).then(a.identity.pid.cmp(&b.identity.pid))
    });
    for index in rest {
        visit(index, &processes, &children_by_parent, &mut visited, &mut ordered);
    }
    let mut slots: Vec<Option<ResourceTelemetryProcess>> = processes.into_iter().map(Some).collect();
    ordered.into_iter().filter_map(|index| slots[index].take()).collect()
}

fn delta(current: i64, previous: i64, elapsed_ms: i64) -> i64 {
    if elapsed_ms <= 0 || elapsed_ms > MAX_DELTA_INTERVAL_MS || current < previous {
        0
    } else {
        current - previous
    }
}

impl GroupCounters {
    fn add(&mut self, cpu: i64, read: i64, write: i64, starts: i64, exits: i64) {
        self.cpu_time_ms += cpu;
        self.io_read_bytes += read;
        self.io_write_bytes += write;
        self.process_starts += starts;
        self.process_exits += exits;
    }
}

impl TelemetryCounters {
    fn group_mut(&mut self, group: Group) -> &mut GroupCounters {
        match group {
            Group::Backend => &mut self.backend,
            Group::Electron => &mut self.electron,
            Group::Monitor => &mut self.monitor,
        }
    }

    fn add(&mut self, category: ResourceTelemetryProcessCategory, cpu: i64, read: i64, write: i64, starts: i64, exits: i64) {
        self.group_mut(category_group(category)).add(cpu, read, write, starts, exits);
        self.all_t3.add(cpu, read, write, starts, exits);
    }
}

fn apply_lifecycle_counters(
    counters: TelemetryCounters,
    deltas: &[ProcessDelta],
    current: &HashMap<String, ProcessState>,
    current_order: &[String],
    previous: &HashMap<String, ProcessState>,
) -> TelemetryCounters {
    let mut counters = counters;
    for d in deltas {
        counters.add(d.category, d.cpu_time_ms, d.io_read_bytes, d.io_write_bytes, 0, 0);
    }
    for key in current_order {
        if previous.contains_key(key) {
            continue;
        }
        if let Some(state) = current.get(key) {
            counters.add(state.process.category, 0, 0, 0, 1, 0);
        }
    }
    for (key, state) in previous {
        if current.contains_key(key) {
            continue;
        }
        counters.add(state.process.category, 0, 0, 0, 0, 1);
    }
    counters
}

fn aggregate<'a>(processes: impl Iterator<Item = &'a ResourceTelemetryProcess>, counters: GroupCounters) -> ResourceTelemetryAggregate {
    let mut count = 0;
    let (mut cpu, mut rss, mut peak, mut read_rate, mut write_rate) = (0.0, 0i64, 0i64, 0.0, 0.0);
    for process in processes {
        count += 1;
        cpu += process.cpu_percent.0;
        rss += process.resident_bytes;
        peak += process.peak_resident_bytes;
        read_rate += process.io_read_bytes_per_second.0;
        write_rate += process.io_write_bytes_per_second.0;
    }
    ResourceTelemetryAggregate {
        process_count: count,
        current_cpu_percent: JsNumber(cpu),
        cpu_time_ms: counters.cpu_time_ms,
        current_rss_bytes: rss,
        peak_rss_bytes: peak,
        io_read_bytes: counters.io_read_bytes,
        io_write_bytes: counters.io_write_bytes,
        io_read_bytes_per_second: JsNumber(read_rate),
        io_write_bytes_per_second: JsNumber(write_rate),
        process_starts: counters.process_starts,
        process_exits: counters.process_exits,
    }
}

fn io_semantics(value: ResourceMonitorProcessSampleIoSemantics) -> ResourceTelemetryIoSemantics {
    match value {
        ResourceMonitorProcessSampleIoSemantics::Storage => ResourceTelemetryIoSemantics::Storage,
        ResourceMonitorProcessSampleIoSemantics::AllIo => ResourceTelemetryIoSemantics::AllIo,
    }
}

fn date(ms: i64) -> DateTimeUtc {
    DateTimeUtc::from_millis(ms).unwrap_or_else(|_| DateTimeUtc::now())
}

/// `mergeProcesses`.
pub fn merge_processes(input: MergeProcessesInput<'_>) -> MergeProcessesResult {
    let native_processes: &[ResourceMonitorProcessSample] = input.native_snapshot.map(|s| s.processes.as_slice()).unwrap_or(&[]);
    let electron_metrics: &[DesktopElectronProcessMetric] = input.desktop_snapshot.map(|s| s.electron_processes.as_slice()).unwrap_or(&[]);
    let sampled_at_ms = match (input.native_snapshot, input.desktop_snapshot) {
        (None, None) => input.fallback_sampled_at_ms,
        (None, Some(desktop)) => desktop.sampled_at_unix_ms,
        (Some(native), None) => native.sampled_at_unix_ms,
        (Some(native), Some(desktop)) => native.sampled_at_unix_ms.max(desktop.sampled_at_unix_ms),
    };
    let native_sampled_at_ms = input.native_snapshot.map(|s| s.sampled_at_unix_ms);
    let desktop_sampled_at_ms = input.desktop_snapshot.map(|s| s.sampled_at_unix_ms);
    let native_pids: HashSet<i64> = native_processes.iter().map(|p| p.pid).collect();

    // Insertion-ordered "map" by pid: native rows first, then synthetic Electron rows.
    let mut processes: Vec<ResourceMonitorProcessSample> = Vec::with_capacity(native_processes.len());
    let mut position_by_pid: HashMap<i64, usize> = HashMap::new();
    for process in native_processes {
        match position_by_pid.get(&process.pid) {
            Some(index) => processes[*index] = process.clone(),
            None => {
                position_by_pid.insert(process.pid, processes.len());
                processes.push(process.clone());
            }
        }
    }
    let mut metrics_by_pid: HashMap<i64, &DesktopElectronProcessMetric> = HashMap::new();
    for metric in electron_metrics {
        match position_by_pid.get(&metric.pid) {
            None => {
                let previous = input.previous.get(&process_identity_key(metric.pid, metric.creation_time_ms));
                let sample = synthetic_native_sample(metric, desktop_sampled_at_ms.unwrap_or(sampled_at_ms), previous);
                position_by_pid.insert(metric.pid, processes.len());
                processes.push(sample);
                metrics_by_pid.insert(metric.pid, metric);
            }
            Some(index) => {
                if (metric.creation_time_ms - processes[*index].start_time_ms).abs() <= ELECTRON_IDENTITY_TOLERANCE_MS {
                    metrics_by_pid.insert(metric.pid, metric);
                }
            }
        }
    }
    let by_pid: HashMap<i64, &ResourceMonitorProcessSample> = processes.iter().map(|p| (p.pid, p)).collect();

    let mut requested_roots: Vec<i64> = Vec::new();
    for pid in input.electron_root_pids.unwrap_or(&[]) {
        if !requested_roots.contains(pid) {
            requested_roots.push(*pid);
        }
    }
    let explicit_roots: Vec<i64> = requested_roots
        .into_iter()
        .filter(|pid| {
            let Some(process) = by_pid.get(pid) else {
                return false;
            };
            if let Some(expected) = input.electron_root_start_times.and_then(|times| times.get(pid)) {
                return (process.start_time_ms - expected).abs() <= ELECTRON_IDENTITY_TOLERANCE_MS;
            }
            if metrics_by_pid.contains_key(pid) {
                return true;
            }
            input.previous.values().any(|previous| {
                previous.process.category == ResourceTelemetryProcessCategory::ElectronMain
                    && previous.process.identity.pid == *pid
                    && previous.process.identity.start_time_ms == process.start_time_ms
            })
        })
        .collect();
    let explicit_set: HashSet<i64> = explicit_roots.iter().copied().collect();
    let electron_pids: HashSet<i64> = metrics_by_pid.keys().copied().chain(explicit_roots.iter().copied()).collect();
    let mut implicit_roots: Vec<i64> = electron_pids
        .iter()
        .copied()
        .filter(|pid| {
            if explicit_set.contains(pid) {
                return false;
            }
            match by_pid.get(pid) {
                None => true,
                Some(process) => !has_electron_ancestor(process, &by_pid, &electron_pids),
            }
        })
        .collect();
    implicit_roots.sort_unstable();
    let mut electron_root_pids: Vec<i64> = Vec::new();
    for pid in explicit_roots.iter().chain(implicit_roots.iter()) {
        if !electron_root_pids.contains(pid) {
            electron_root_pids.push(*pid);
        }
    }
    let mut root_pids = vec![input.server_pid];
    root_pids.extend(electron_root_pids.iter().copied());
    let depths = process_depths(&processes, &root_pids);
    let mut children_by_parent: HashMap<i64, Vec<i64>> = HashMap::new();
    for process in &processes {
        children_by_parent.entry(process.ppid).or_default().push(process.pid);
    }

    let mut next_previous: HashMap<String, ProcessState> = HashMap::new();
    let mut next_order: Vec<String> = Vec::new();
    let mut deltas: Vec<ProcessDelta> = Vec::new();
    let mut normalized: Vec<ResourceTelemetryProcess> = Vec::with_capacity(processes.len());
    for process in &processes {
        let identity_key = process_identity_key(process.pid, process.start_time_ms);
        let previous = input.previous.get(&identity_key);
        let counter_sampled_at_ms = if native_pids.contains(&process.pid) {
            native_sampled_at_ms.unwrap_or(sampled_at_ms)
        } else {
            desktop_sampled_at_ms.unwrap_or(sampled_at_ms)
        };
        let elapsed_ms = previous.map(|p| counter_sampled_at_ms - p.sampled_at_ms).unwrap_or(0);
        let (cpu_delta, read_delta, write_delta) = match previous {
            Some(p) => (
                delta(process.cpu_time_ms, p.process.cpu_time_ms, elapsed_ms),
                delta(process.io_read_bytes, p.process.io_read_bytes, elapsed_ms),
                delta(process.io_write_bytes, p.process.io_write_bytes, elapsed_ms),
            ),
            None => (0, 0, 0),
        };
        let electron_metric = match_electron_metric(process, &metrics_by_pid);
        let category = if process.pid == input.server_pid {
            ResourceTelemetryProcessCategory::Server
        } else if input.sidecar_pid == Some(process.pid) {
            ResourceTelemetryProcessCategory::ResourceMonitor
        } else if explicit_set.contains(&process.pid) {
            ResourceTelemetryProcessCategory::ElectronMain
        } else if let Some(metric) = electron_metric {
            electron_category(metric)
        } else if is_electron_descendant(process.pid, &by_pid, &electron_pids) {
            inferred_electron_category(process)
        } else {
            ResourceTelemetryProcessCategory::ServerChild
        };
        let first_seen_at = previous.map(|p| p.process.first_seen_at).unwrap_or_else(|| date(sampled_at_ms));
        let preserve_previous_rates = !input.update_previous && previous.is_some();
        let cpu_percent = match previous {
            Some(p) if preserve_previous_rates => p.process.cpu_percent.0,
            Some(_) if elapsed_ms > 0 && elapsed_ms <= MAX_DELTA_INTERVAL_MS => (cpu_delta as f64 / elapsed_ms as f64) * 100.0,
            _ => finite_non_negative(process.cpu_percent.0),
        };
        let rate = |previous_rate: Option<f64>, d: i64| -> f64 {
            match previous_rate {
                Some(rate) if preserve_previous_rates => rate,
                _ if elapsed_ms > 0 => finite_non_negative((d as f64 * 1_000.0) / elapsed_ms as f64),
                _ => 0.0,
            }
        };
        let mut child_pids = children_by_parent.get(&process.pid).cloned().unwrap_or_default();
        child_pids.sort_unstable();
        let normalized_process = ResourceTelemetryProcess {
            identity: ResourceTelemetryProcessIdentity {
                pid: process.pid,
                start_time_ms: process.start_time_ms,
            },
            ppid: process.ppid,
            child_pids,
            depth: depths.get(&process.pid).copied().unwrap_or(0),
            name: process.name.clone(),
            command: process.command.clone(),
            status: process.status.clone(),
            category,
            electron_type: electron_metric.map(|m| m.r#type),
            electron_service_name: electron_metric.and_then(|m| m.service_name.clone()).filter(|s| !s.is_empty()),
            cpu_percent: JsNumber(finite_non_negative(cpu_percent)),
            cpu_time_ms: process.cpu_time_ms,
            resident_bytes: process.resident_bytes,
            peak_resident_bytes: process
                .resident_bytes
                .max(electron_metric.map(|m| m.peak_working_set_bytes).unwrap_or(0))
                .max(previous.map(|p| p.process.peak_resident_bytes).unwrap_or(0)),
            virtual_bytes: process.virtual_bytes,
            io_read_bytes: process.io_read_bytes,
            io_write_bytes: process.io_write_bytes,
            io_read_bytes_per_second: JsNumber(rate(previous.map(|p| p.process.io_read_bytes_per_second.0), read_delta)),
            io_write_bytes_per_second: JsNumber(rate(previous.map(|p| p.process.io_write_bytes_per_second.0), write_delta)),
            io_semantics: io_semantics(process.io_semantics),
            idle_wakeups_per_second: electron_metric.map(|m| m.idle_wakeups_per_second),
            run_time_ms: process.run_time_ms,
            first_seen_at,
            last_seen_at: date(sampled_at_ms),
        };
        if !next_previous.contains_key(&identity_key) {
            next_order.push(identity_key.clone());
        }
        next_previous.insert(
            identity_key.clone(),
            ProcessState {
                process: normalized_process.clone(),
                sampled_at_ms: counter_sampled_at_ms,
            },
        );
        deltas.push(ProcessDelta {
            identity_key,
            category,
            cpu_time_ms: cpu_delta,
            io_read_bytes: read_delta,
            io_write_bytes: write_delta,
        });
        normalized.push(normalized_process);
    }
    let ordered = order_process_tree(normalized, &root_pids);

    let counters = if input.update_previous {
        apply_lifecycle_counters(input.counters, &deltas, &next_previous, &next_order, input.previous)
    } else {
        input.counters
    };
    let in_group = |group: Group| ordered.iter().filter(move |p| category_group(p.category) == group);
    let groups = ResourceTelemetryGroups {
        backend: aggregate(in_group(Group::Backend), counters.backend),
        electron: aggregate(in_group(Group::Electron), counters.electron),
        monitor: aggregate(in_group(Group::Monitor), counters.monitor),
        all_t3: aggregate(ordered.iter(), counters.all_t3),
    };
    MergeProcessesResult {
        sampled_at_ms,
        previous: if input.update_previous { next_previous } else { input.previous.clone() },
        processes: ordered,
        counters,
        groups,
        deltas,
    }
}
