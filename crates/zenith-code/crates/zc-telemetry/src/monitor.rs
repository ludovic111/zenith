//! The resource monitor of `code/native/resource-monitor` (`t3-resource-monitor`, protocol 3),
//! linked in rather than run as a sidecar: the same `sysinfo` collector, process-tree
//! selection, start-time identity checks and bounded history, on a dedicated thread that
//! takes the protocol's commands over a channel and answers with its events.
//!
//! Differences with the sidecar, all invisible on the wire: commands and events are typed
//! values instead of NDJSON lines, a history read answers in one chunk, and the monitor's pid
//! is the server's own (its sampling cost is part of the server process).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind, MINIMUM_CPU_UPDATE_INTERVAL};
use zc_contracts::{
    JsNumber, Lit3, LitSnapshot, ResourceMonitorCapabilities, ResourceMonitorExternalProcess, ResourceMonitorProcessSample,
    ResourceMonitorProcessSampleIoSemantics, ResourceMonitorProcessTableEntry, ResourceMonitorSnapshotEvent,
};

pub const PROTOCOL_VERSION: i64 = 3;
pub const MONITOR_VERSION: &str = env!("CARGO_PKG_VERSION");
const MIN_SAMPLE_INTERVAL_MS: u64 = 250;
const MAX_SAMPLE_INTERVAL_MS: u64 = 60_000;
const PROCESS_START_TIME_PRECISION_MS: u64 = 1_000;
pub const HISTORY_RETENTION_MS: u64 = 60 * 60_000;
pub const MAX_HISTORY_SNAPSHOTS: usize = 3_600;
const INPUT_QUEUE_CAPACITY: usize = 64;
const MAX_HISTORY_RETAINED_ENTRIES: usize = 20_000;
const MAX_HISTORY_RETAINED_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_PROCESS_NAME_BYTES: usize = 1_024;
const MAX_PROCESS_COMMAND_BYTES: usize = 16 * 1_024;
const MAX_PROCESS_STATUS_BYTES: usize = 256;

/// The protocol's commands (without `version`).
#[derive(Debug, Clone)]
pub enum MonitorCommand {
    Configure {
        root_pid: u32,
        sample_interval_ms: u64,
        external_processes: Vec<ResourceMonitorExternalProcess>,
    },
    SetExternalProcesses(Vec<ResourceMonitorExternalProcess>),
    SetSampleInterval(u64),
    SetStreaming(bool),
    SampleNow {
        request_id: String,
    },
    ProcessTable {
        request_id: String,
    },
    ReadHistory {
        request_id: String,
        window_ms: u64,
    },
    Shutdown,
}

/// The protocol's events.
#[derive(Debug, Clone)]
pub enum MonitorEvent {
    Hello {
        sidecar_version: &'static str,
        sidecar_pid: u32,
        platform: &'static str,
        arch: &'static str,
        capabilities: ResourceMonitorCapabilities,
    },
    Snapshot(ResourceMonitorSnapshotEvent),
    ProcessTable {
        request_id: String,
        processes: Vec<ResourceMonitorProcessTableEntry>,
    },
    HistoryChunk {
        request_id: String,
        done: bool,
        snapshots: Vec<ResourceMonitorSnapshotEvent>,
    },
    Error {
        code: &'static str,
        message: String,
        recoverable: bool,
    },
}

/// Where the monitor sends its events. `false` means nobody listens any more (it stops).
pub trait EventSink: Send + 'static {
    fn send(&mut self, event: MonitorEvent) -> bool;
}

impl EventSink for tokio::sync::mpsc::UnboundedSender<MonitorEvent> {
    fn send(&mut self, event: MonitorEvent) -> bool {
        tokio::sync::mpsc::UnboundedSender::send(self, event).is_ok()
    }
}

/// A running monitor thread.
pub struct MonitorHandle {
    commands: SyncSender<MonitorCommand>,
}

impl MonitorHandle {
    /// Queues a command; fails when the queue is full or the thread is gone.
    pub fn send(&self, command: MonitorCommand) -> Result<(), String> {
        self.commands.try_send(command).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => "the resource monitor command queue is full".to_owned(),
            mpsc::TrySendError::Disconnected(_) => "the resource monitor is not running".to_owned(),
        })
    }
}

/// Starts the monitor thread; it says hello first and stops on `Shutdown`, when every
/// [`MonitorHandle`] is dropped, or when `events` stops accepting.
pub fn spawn(events: impl EventSink) -> std::io::Result<MonitorHandle> {
    let (commands, receiver) = mpsc::sync_channel(INPUT_QUEUE_CAPACITY);
    thread::Builder::new()
        .name("zenith-resource-monitor".into())
        .spawn(move || run(receiver, events))?;
    Ok(MonitorHandle { commands })
}

#[derive(Debug, Clone)]
struct CollectorConfig {
    root_pid: u32,
    sample_interval: Option<Duration>,
    external_processes: HashMap<u32, Option<u64>>,
}

fn external_map(processes: Vec<ResourceMonitorExternalProcess>) -> HashMap<u32, Option<u64>> {
    processes
        .into_iter()
        .filter_map(|p| Some((u32::try_from(p.pid).ok()?, p.start_time_ms.and_then(|s| u64::try_from(s).ok()))))
        .collect()
}

fn run(receiver: Receiver<MonitorCommand>, mut events: impl EventSink) {
    if !events.send(MonitorEvent::Hello {
        sidecar_version: MONITOR_VERSION,
        sidecar_pid: std::process::id(),
        platform: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        capabilities: ResourceMonitorCapabilities {
            cumulative_cpu_time: true,
            current_cpu_percent: true,
            resident_memory: true,
            virtual_memory: true,
            io_bytes: true,
            process_start_time: true,
            process_tree: true,
        },
    }) {
        return;
    }
    let mut collector = Collector::new();
    let mut history = HistoryRecorder::default();
    let mut config: Option<CollectorConfig> = None;
    let mut next_sample_at: Option<Instant> = None;
    let mut streaming_enabled = false;

    loop {
        if next_sample_at.is_some_and(|deadline| deadline <= Instant::now()) {
            match config.as_ref().and_then(|c| c.sample_interval.map(|i| (c, i))) {
                Some((current, interval)) => {
                    let event = collector.sample(current, None);
                    history.record(&event);
                    if streaming_enabled && !events.send(MonitorEvent::Snapshot(event)) {
                        return;
                    }
                    next_sample_at = Some(Instant::now() + interval);
                }
                None => next_sample_at = None,
            }
            continue;
        }
        let timeout = next_sample_at
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_secs(60));
        let command = match receiver.recv_timeout(timeout) {
            Ok(command) => command,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let not_configured = |message: &str| MonitorEvent::Error {
            code: "not-configured",
            message: message.to_owned(),
            recoverable: true,
        };
        let sent = match command {
            MonitorCommand::Configure {
                root_pid,
                sample_interval_ms,
                external_processes,
            } => {
                let sample_interval = clamp_sample_interval(sample_interval_ms);
                config = Some(CollectorConfig {
                    root_pid,
                    sample_interval,
                    external_processes: external_map(external_processes),
                });
                collector.prime_cpu_usage();
                next_sample_at = sample_interval.map(|_| Instant::now());
                true
            }
            MonitorCommand::SetExternalProcesses(processes) => match config.as_mut() {
                Some(current) => {
                    current.external_processes = external_map(processes);
                    true
                }
                None => events.send(not_configured("configure must be sent before external processes")),
            },
            MonitorCommand::SetSampleInterval(ms) => match config.as_mut() {
                Some(current) => {
                    current.sample_interval = clamp_sample_interval(ms);
                    next_sample_at = current.sample_interval.map(|interval| Instant::now() + interval);
                    true
                }
                None => events.send(not_configured("configure must be sent before changing the sample interval")),
            },
            MonitorCommand::SetStreaming(enabled) => {
                streaming_enabled = enabled;
                true
            }
            MonitorCommand::SampleNow { request_id } => match config.as_ref() {
                Some(current) => {
                    let event = collector.sample(current, Some(request_id));
                    history.record(&event);
                    next_sample_at = sample_now_deadline(next_sample_at, current.sample_interval, Instant::now());
                    events.send(MonitorEvent::Snapshot(event))
                }
                None => events.send(not_configured("configure must be sent before sampling")),
            },
            MonitorCommand::ProcessTable { request_id } => events.send(MonitorEvent::ProcessTable {
                request_id,
                processes: collector.process_table(),
            }),
            MonitorCommand::ReadHistory { request_id, window_ms } => {
                if config.is_some() {
                    events.send(MonitorEvent::HistoryChunk {
                        request_id,
                        done: true,
                        snapshots: history.read(window_ms, unix_time_ms()),
                    })
                } else {
                    events.send(not_configured("configure must be sent before reading history"))
                }
            }
            MonitorCommand::Shutdown => return,
        };
        if !sent {
            return;
        }
    }
}

/// Snapshots retained for `readHistory`: one hour, 3,600 snapshots, 20,000 process entries
/// and an estimated 64 MiB at most.
#[derive(Default)]
pub struct HistoryRecorder {
    snapshots: VecDeque<ResourceMonitorSnapshotEvent>,
    retained_entry_count: usize,
    retained_bytes: usize,
}

fn retained_entry_count(snapshot: &ResourceMonitorSnapshotEvent) -> usize {
    snapshot.processes.len() + snapshot.external_processes.as_ref().map_or(0, Vec::len)
}

fn estimated_history_bytes(snapshot: &ResourceMonitorSnapshotEvent) -> usize {
    std::mem::size_of::<ResourceMonitorSnapshotEvent>()
        + snapshot
            .processes
            .iter()
            .map(|p| std::mem::size_of::<ResourceMonitorProcessSample>() + p.name.len() + p.command.len() + p.status.len())
            .sum::<usize>()
        + snapshot
            .external_processes
            .as_ref()
            .map_or(0, |e| e.len() * std::mem::size_of::<ResourceMonitorExternalProcess>())
}

impl HistoryRecorder {
    pub fn record(&mut self, snapshot: &ResourceMonitorSnapshotEvent) {
        self.record_with_limits(snapshot, MAX_HISTORY_SNAPSHOTS, MAX_HISTORY_RETAINED_ENTRIES, MAX_HISTORY_RETAINED_BYTES);
    }

    pub fn record_with_limits(&mut self, snapshot: &ResourceMonitorSnapshotEvent, max_snapshots: usize, max_entries: usize, max_bytes: usize) {
        let clock_moved_backward = self
            .snapshots
            .back()
            .is_some_and(|previous| previous.sampled_at_unix_ms > snapshot.sampled_at_unix_ms);
        let mut retained = snapshot.clone();
        retained.request_id = None;
        self.retained_entry_count += retained_entry_count(&retained);
        self.retained_bytes += estimated_history_bytes(&retained);
        self.snapshots.push_back(retained);
        let now_ms = snapshot.sampled_at_unix_ms;
        if clock_moved_backward {
            let (mut entries, mut bytes) = (0, 0);
            self.snapshots.retain(|s| {
                let keep = s.sampled_at_unix_ms <= now_ms;
                if !keep {
                    entries += retained_entry_count(s);
                    bytes += estimated_history_bytes(s);
                }
                keep
            });
            self.retained_entry_count = self.retained_entry_count.saturating_sub(entries);
            self.retained_bytes = self.retained_bytes.saturating_sub(bytes);
        }
        let oldest_kept = now_ms.saturating_sub(HISTORY_RETENTION_MS as i64);
        while self.snapshots.front().is_some_and(|front| {
            front.sampled_at_unix_ms < oldest_kept
                || self.snapshots.len() > max_snapshots
                || self.retained_entry_count > max_entries
                || self.retained_bytes > max_bytes
        }) {
            if let Some(removed) = self.snapshots.pop_front() {
                self.retained_entry_count = self.retained_entry_count.saturating_sub(retained_entry_count(&removed));
                self.retained_bytes = self.retained_bytes.saturating_sub(estimated_history_bytes(&removed));
            }
        }
    }

    pub fn read(&self, window_ms: u64, now_ms: u64) -> Vec<ResourceMonitorSnapshotEvent> {
        let started_at_ms = now_ms.saturating_sub(window_ms.min(HISTORY_RETENTION_MS)) as i64;
        let now_ms = now_ms as i64;
        self.snapshots
            .iter()
            .filter(|s| s.sampled_at_unix_ms >= started_at_ms && s.sampled_at_unix_ms <= now_ms)
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.snapshots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.snapshots.is_empty()
    }

    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    pub fn retained_entries(&self) -> usize {
        self.retained_entry_count
    }

    pub fn front_sequence(&self) -> Option<i64> {
        self.snapshots.front().map(|s| s.sequence)
    }
}

struct Collector {
    system: System,
    sequence: i64,
    cpu_baseline_refreshed_at: Option<Instant>,
}

impl Collector {
    fn new() -> Self {
        Self {
            system: System::new(),
            sequence: 0,
            cpu_baseline_refreshed_at: None,
        }
    }

    fn prime_cpu_usage(&mut self) {
        self.system
            .refresh_processes_specifics(ProcessesToUpdate::All, true, process_discovery_refresh_kind());
        self.cpu_baseline_refreshed_at = Some(Instant::now());
    }

    fn process_table(&self) -> Vec<ResourceMonitorProcessTableEntry> {
        // A dedicated System, so this refresh cannot reset the CPU baseline of the snapshots.
        let mut system = System::new();
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().without_tasks());
        let mut processes: Vec<ResourceMonitorProcessTableEntry> = system
            .processes()
            .iter()
            .filter_map(|(pid, process)| {
                let pid = pid.as_u32();
                // Pid 0 (the kernel idle process on some platforms) is not a positive pid.
                (pid != 0).then(|| ResourceMonitorProcessTableEntry {
                    pid: i64::from(pid),
                    ppid: i64::from(process.parent().map(Pid::as_u32).unwrap_or(0)),
                    name: truncate_utf8(process.name().to_string_lossy().into_owned(), MAX_PROCESS_NAME_BYTES),
                })
            })
            .collect();
        processes.sort_by_key(|p| p.pid);
        processes
    }

    fn sample(&mut self, config: &CollectorConfig, request_id: Option<String>) -> ResourceMonitorSnapshotEvent {
        if let Some(delay) = remaining_cpu_measurement_delay(self.cpu_baseline_refreshed_at.take(), Instant::now()) {
            thread::sleep(delay);
        }
        let collection_started = Instant::now();
        self.system
            .refresh_processes_specifics(ProcessesToUpdate::All, true, process_discovery_refresh_kind());
        self.cpu_baseline_refreshed_at = Some(Instant::now());

        let rows: Vec<(u32, u32, u64)> = self
            .system
            .processes()
            .iter()
            .map(|(pid, process)| {
                (
                    pid.as_u32(),
                    process.parent().map(Pid::as_u32).unwrap_or(0),
                    process.start_time().saturating_mul(1_000),
                )
            })
            .collect();
        let mut external_processes: Vec<ResourceMonitorExternalProcess> = config
            .external_processes
            .iter()
            .filter_map(|(pid, expected)| {
                let (_, _, actual) = rows.iter().find(|(candidate, _, _)| candidate == pid)?;
                matches_external_identity(*actual, *expected).then(|| ResourceMonitorExternalProcess {
                    pid: i64::from(*pid),
                    start_time_ms: Some(*actual as i64),
                })
            })
            .collect();
        external_processes.sort_by_key(|p| p.pid);
        let mut roots: HashSet<u32> = external_processes.iter().filter_map(|p| u32::try_from(p.pid).ok()).collect();
        roots.insert(config.root_pid);
        let tracked = select_tracked_pids(&rows, &roots);
        let tracked_count = tracked.len();
        let process_details = if cfg!(target_os = "linux") && !tracked.is_empty() {
            let monitor_pid = Pid::from_u32(std::process::id());
            let mut detail_pids: Vec<Pid> = tracked.iter().copied().map(Pid::from_u32).collect();
            if !tracked.contains(&monitor_pid.as_u32()) {
                detail_pids.push(monitor_pid);
            }
            // Detail fields need no baseline. Drop command data and OS handles after each sample.
            let mut details = System::new();
            details.refresh_processes_specifics(ProcessesToUpdate::Some(&detail_pids), true, process_refresh_kind().without_cpu());
            // This process cannot be replaced during collection: its start time exposes any
            // boot-epoch shift between the two System instances.
            let offset = self.system.process(monitor_pid).and_then(|process| {
                details
                    .process(monitor_pid)
                    .map(|detail| i128::from(detail.start_time()) - i128::from(process.start_time()))
            });
            Some((details, offset))
        } else {
            None
        };
        let sample_details = process_details.as_ref().map_or(&self.system, |(details, _)| details);
        let start_time_offset = process_details.as_ref().map_or(Some(0), |(_, offset)| *offset);
        let mut processes: Vec<ResourceMonitorProcessSample> = tracked
            .into_iter()
            .filter_map(|pid| {
                let process = self.system.process(Pid::from_u32(pid))?;
                let details = sample_details.process(Pid::from_u32(pid))?;
                if !matches_process_start_time(process.start_time(), details.start_time(), start_time_offset?) {
                    return None;
                }
                let disk_usage = details.disk_usage();
                let command = if details.cmd().is_empty() {
                    process.name().to_string_lossy().into_owned()
                } else {
                    details.cmd().iter().map(|part| part.to_string_lossy()).collect::<Vec<_>>().join(" ")
                };
                Some(ResourceMonitorProcessSample {
                    pid: i64::from(pid),
                    ppid: i64::from(process.parent().map(Pid::as_u32).unwrap_or(0)),
                    start_time_ms: process.start_time().saturating_mul(1_000) as i64,
                    run_time_ms: process.run_time().saturating_mul(1_000) as i64,
                    name: truncate_utf8(process.name().to_string_lossy().into_owned(), MAX_PROCESS_NAME_BYTES),
                    command: truncate_utf8(command, MAX_PROCESS_COMMAND_BYTES),
                    status: truncate_utf8(format!("{:?}", process.status()), MAX_PROCESS_STATUS_BYTES),
                    cpu_percent: JsNumber(f64::from(process.cpu_usage())),
                    cpu_time_ms: process.accumulated_cpu_time() as i64,
                    resident_bytes: details.memory() as i64,
                    virtual_bytes: details.virtual_memory() as i64,
                    io_read_bytes: disk_usage.total_read_bytes as i64,
                    io_write_bytes: disk_usage.total_written_bytes as i64,
                    io_semantics: io_semantics(),
                })
            })
            .collect();
        drop(process_details);
        processes.sort_by_key(|p| p.pid);
        self.sequence = self.sequence.saturating_add(1);
        let retained = processes.len();
        ResourceMonitorSnapshotEvent {
            version: Lit3,
            r#type: LitSnapshot,
            sequence: self.sequence,
            sampled_at_unix_ms: unix_time_ms() as i64,
            collection_duration_micros: collection_started.elapsed().as_micros() as i64,
            scanned_process_count: self.system.processes().len() as i64,
            retained_process_count: retained as i64,
            inaccessible_process_count: inaccessible_process_count(tracked_count, retained) as i64,
            request_id,
            external_processes: Some(external_processes),
            processes,
        }
    }
}

// Keep CPU baselines separate: even a metadata refresh resets Linux process times.
fn process_discovery_refresh_kind() -> ProcessRefreshKind {
    if cfg!(target_os = "linux") {
        ProcessRefreshKind::nothing().with_cpu().without_tasks()
    } else {
        process_refresh_kind()
    }
}

pub fn process_refresh_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing()
        .with_memory()
        .with_cpu()
        .with_disk_usage()
        .with_cmd(UpdateKind::Always)
        .without_tasks()
}

pub fn inaccessible_process_count(selected: usize, materialized: usize) -> usize {
    selected.saturating_sub(materialized)
}

pub fn matches_process_start_time(discovered: u64, detail: u64, epoch_offset: i128) -> bool {
    i128::from(detail) - epoch_offset == i128::from(discovered)
}

pub fn remaining_cpu_measurement_delay(baseline_refreshed_at: Option<Instant>, now: Instant) -> Option<Duration> {
    baseline_refreshed_at
        .and_then(|baseline| MINIMUM_CPU_UPDATE_INTERVAL.checked_sub(now.duration_since(baseline)))
        .filter(|delay| !delay.is_zero())
}

/// sysinfo reports process starts in whole seconds: the higher-resolution expected time is
/// normalized to that bucket instead of accepting adjacent seconds (a quickly reused pid).
pub fn matches_external_identity(actual_start_time_ms: u64, expected_start_time_ms: Option<u64>) -> bool {
    expected_start_time_ms.is_none_or(|expected| actual_start_time_ms == expected - (expected % PROCESS_START_TIME_PRECISION_MS))
}

/// The roots and every descendant not older than its parent (a reused parent pid).
pub fn select_tracked_pids(rows: &[(u32, u32, u64)], roots: &HashSet<u32>) -> HashSet<u32> {
    let mut children_by_parent: HashMap<u32, Vec<(u32, u64)>> = HashMap::new();
    let mut start_time_by_pid: HashMap<u32, u64> = HashMap::new();
    for (pid, ppid, start_time_ms) in rows {
        children_by_parent.entry(*ppid).or_default().push((*pid, *start_time_ms));
        start_time_by_pid.insert(*pid, *start_time_ms);
    }
    let mut tracked = HashSet::new();
    let mut visited = HashSet::new();
    let mut queue: VecDeque<(u32, u64)> = roots.iter().filter_map(|pid| start_time_by_pid.get(pid).map(|start| (*pid, *start))).collect();
    while let Some((pid, start_time_ms)) = queue.pop_front() {
        if !visited.insert((pid, start_time_ms)) {
            continue;
        }
        tracked.insert(pid);
        if let Some(children) = children_by_parent.get(&pid) {
            queue.extend(children.iter().copied().filter(|(_, child_start)| *child_start >= start_time_ms));
        }
    }
    tracked
}

pub fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }
    value.truncate(boundary);
    value
}

fn io_semantics() -> ResourceMonitorProcessSampleIoSemantics {
    if cfg!(target_os = "windows") {
        ResourceMonitorProcessSampleIoSemantics::AllIo
    } else {
        ResourceMonitorProcessSampleIoSemantics::Storage
    }
}

pub fn unix_time_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

pub fn clamp_sample_interval(sample_interval_ms: u64) -> Option<Duration> {
    (sample_interval_ms > 0).then(|| Duration::from_millis(sample_interval_ms.clamp(MIN_SAMPLE_INTERVAL_MS, MAX_SAMPLE_INTERVAL_MS)))
}

pub fn sample_now_deadline(current: Option<Instant>, interval: Option<Duration>, now: Instant) -> Option<Instant> {
    current.or_else(|| interval.map(|duration| now + duration))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(sequence: i64, sampled_at: i64) -> ResourceMonitorSnapshotEvent {
        ResourceMonitorSnapshotEvent {
            version: Lit3,
            r#type: LitSnapshot,
            sequence,
            sampled_at_unix_ms: sampled_at,
            collection_duration_micros: 1,
            scanned_process_count: 0,
            retained_process_count: 0,
            inaccessible_process_count: 0,
            request_id: None,
            external_processes: Some(Vec::new()),
            processes: Vec::new(),
        }
    }

    fn process(pid: i64, start: i64, command: String) -> ResourceMonitorProcessSample {
        ResourceMonitorProcessSample {
            pid,
            ppid: 0,
            start_time_ms: start,
            run_time_ms: 0,
            name: "process".into(),
            command,
            status: "Run".into(),
            cpu_percent: JsNumber(0.0),
            cpu_time_ms: 0,
            resident_bytes: 0,
            virtual_bytes: 0,
            io_read_bytes: 0,
            io_write_bytes: 0,
            io_semantics: ResourceMonitorProcessSampleIoSemantics::Storage,
        }
    }

    #[test]
    fn selects_roots_and_all_descendants() {
        let rows = vec![
            (10, 1, 1_000),
            (11, 10, 1_100),
            (12, 11, 1_200),
            (20, 1, 2_000),
            (21, 20, 2_100),
            (30, 99, 3_000),
        ];
        assert_eq!(select_tracked_pids(&rows, &HashSet::from([10, 20])), HashSet::from([10, 11, 12, 20, 21]));
    }

    #[test]
    fn rejects_descendants_older_than_a_reused_parent_pid() {
        let rows = vec![(20, 1, 5_000), (21, 20, 4_000), (22, 20, 5_100), (23, 21, 5_200)];
        assert_eq!(select_tracked_pids(&rows, &HashSet::from([20])), HashSet::from([20, 22]));
    }

    #[test]
    fn ignores_missing_roots() {
        let rows = vec![(10, 1, 1_000), (11, 10, 1_100)];
        assert!(select_tracked_pids(&rows, &HashSet::from([99])).is_empty());
    }

    #[test]
    fn validates_external_process_start_identity() {
        assert!(matches_external_identity(10_000, None));
        assert!(matches_external_identity(10_000, Some(10_999)));
        assert!(!matches_external_identity(10_000, Some(11_000)));
        assert!(!matches_external_identity(10_000, Some(9_999)));
    }

    #[test]
    fn loads_details_when_an_existing_process_becomes_selected() {
        let mut collector = Collector::new();
        let mut config = CollectorConfig {
            root_pid: u32::MAX,
            sample_interval: None,
            external_processes: HashMap::new(),
        };
        assert!(collector.sample(&config, None).processes.is_empty());
        config.root_pid = std::process::id();
        let snapshot = collector.sample(&config, None);
        let own = snapshot
            .processes
            .iter()
            .find(|p| p.pid == i64::from(config.root_pid))
            .expect("selected process");
        assert!(!own.command.is_empty());
        assert!(own.resident_bytes > 0);
        assert!(own.cpu_percent.0.is_finite());
        assert_eq!(snapshot.external_processes, Some(Vec::new()));
    }

    #[test]
    fn accepts_clock_shifts_without_accepting_reused_process_starts() {
        assert!(matches_process_start_time(10_000, 13_600, 3_600));
        assert!(!matches_process_start_time(10_000, 13_601, 3_600));
        assert!(matches_process_start_time(10_000, 6_400, -3_600));
        assert!(!matches_process_start_time(10_000, 6_401, -3_600));
        assert!(matches_process_start_time(10_000, 10_000, 0));
        assert!(!matches_process_start_time(10_000, 10_001, 0));
    }

    #[test]
    fn clamps_sample_interval() {
        assert_eq!(clamp_sample_interval(0), None);
        assert_eq!(clamp_sample_interval(1), Some(Duration::from_millis(250)));
        assert_eq!(clamp_sample_interval(100_000), Some(Duration::from_millis(60_000)));
    }

    #[test]
    fn counts_selected_processes_that_could_not_be_materialized() {
        assert_eq!(inaccessible_process_count(5, 3), 2);
        assert_eq!(inaccessible_process_count(3, 5), 0);
    }

    #[test]
    fn waits_for_a_cpu_measurement_window_after_priming() {
        let baseline = Instant::now();
        assert_eq!(remaining_cpu_measurement_delay(Some(baseline), baseline), Some(MINIMUM_CPU_UPDATE_INTERVAL));
        assert_eq!(remaining_cpu_measurement_delay(Some(baseline), baseline + MINIMUM_CPU_UPDATE_INTERVAL), None);
        assert_eq!(remaining_cpu_measurement_delay(None, baseline), None);
    }

    #[test]
    fn retains_bounded_history_without_request_ids() {
        let mut history = HistoryRecorder::default();
        for sequence in 0..=MAX_HISTORY_SNAPSHOTS as i64 {
            let mut s = snapshot(sequence, sequence * 1_000);
            s.request_id = Some("request".into());
            s.external_processes = Some(vec![ResourceMonitorExternalProcess {
                pid: 7,
                start_time_ms: Some(1_000),
            }]);
            history.record(&s);
        }
        assert_eq!(history.len(), MAX_HISTORY_SNAPSHOTS);
        assert!(history.snapshots.iter().all(|s| s.request_id.is_none()));
        assert!(history
            .snapshots
            .iter()
            .all(|s| s.external_processes.as_ref().is_some_and(|e| e.len() == 1 && e[0].pid == 7)));
        assert_eq!(history.read(10_000, MAX_HISTORY_SNAPSHOTS as u64 * 1_000).len(), 11);
    }

    #[test]
    fn excludes_and_trims_future_history_after_the_clock_moves_backward() {
        let mut history = HistoryRecorder::default();
        history.record(&snapshot(1, 2_000));
        assert!(history.read(0, 1_000).is_empty());
        history.record(&snapshot(2, 1_000));
        assert_eq!(history.len(), 1);
        assert_eq!(history.front_sequence(), Some(2));
    }

    #[test]
    fn bounds_history_by_estimated_process_bytes() {
        let mut history = HistoryRecorder::default();
        let one = |sequence: i64| {
            let mut s = snapshot(sequence, sequence * 1_000);
            s.external_processes = None;
            s.processes = vec![process(sequence + 1, sequence * 1_000, "x".repeat(128))];
            s
        };
        let snapshot_bytes = estimated_history_bytes(&one(0));
        for sequence in 0..3 {
            history.record_with_limits(&one(sequence), 3, 3, snapshot_bytes * 2);
        }
        assert!(history.retained_bytes() <= snapshot_bytes * 2);
        assert_eq!(history.len(), 2);
        assert_eq!(history.front_sequence(), Some(1));
    }

    #[test]
    fn counts_external_processes_toward_history_limits() {
        let mut history = HistoryRecorder::default();
        let one = |sequence: i64| {
            let mut s = snapshot(sequence, sequence * 1_000);
            s.external_processes = Some(
                (1..=128)
                    .map(|pid| ResourceMonitorExternalProcess {
                        pid,
                        start_time_ms: Some(pid * 1_000),
                    })
                    .collect(),
            );
            s
        };
        let bytes = estimated_history_bytes(&one(0));
        let entries = retained_entry_count(&one(0));
        for sequence in 0..3 {
            history.record_with_limits(&one(sequence), 3, entries * 2, bytes * 2);
        }
        assert_eq!(history.retained_entries(), entries * 2);
        assert!(history.retained_bytes() <= bytes * 2);
        assert_eq!(history.len(), 2);
        assert_eq!(history.front_sequence(), Some(1));
    }

    #[test]
    fn truncates_process_strings_at_utf8_boundaries() {
        let truncated = truncate_utf8("é".repeat(MAX_PROCESS_NAME_BYTES), MAX_PROCESS_NAME_BYTES - 1);
        assert!(truncated.len() < MAX_PROCESS_NAME_BYTES);
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn refreshes_commands_without_enumerating_linux_tasks() {
        let kind = process_refresh_kind();
        assert_eq!(kind.cmd(), UpdateKind::Always);
        assert!(!kind.tasks());
        assert!(kind.cpu());
        assert!(kind.memory());
        assert!(kind.disk_usage());
    }

    #[test]
    fn sample_now_does_not_postpone_an_existing_periodic_deadline() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(1);
        assert_eq!(
            sample_now_deadline(Some(deadline), Some(Duration::from_secs(5)), now + Duration::from_millis(100)),
            Some(deadline)
        );
    }

    #[test]
    fn the_thread_answers_the_protocol() {
        let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let handle = spawn(events).unwrap();
        assert!(matches!(receiver.blocking_recv(), Some(MonitorEvent::Hello { sidecar_pid, .. }) if sidecar_pid == std::process::id()));
        handle
            .send(MonitorCommand::ReadHistory {
                request_id: "history-1".into(),
                window_ms: 1_000,
            })
            .unwrap();
        assert!(matches!(
            receiver.blocking_recv(),
            Some(MonitorEvent::Error {
                code: "not-configured",
                recoverable: true,
                ..
            })
        ));
        handle
            .send(MonitorCommand::Configure {
                root_pid: std::process::id(),
                sample_interval_ms: 0,
                external_processes: Vec::new(),
            })
            .unwrap();
        handle.send(MonitorCommand::SampleNow { request_id: "sample-1".into() }).unwrap();
        match receiver.blocking_recv() {
            Some(MonitorEvent::Snapshot(snapshot)) => {
                assert_eq!(snapshot.request_id.as_deref(), Some("sample-1"));
                assert!(snapshot.processes.iter().any(|p| p.pid == i64::from(std::process::id())));
            }
            other => panic!("unexpected {other:?}"),
        }
        handle
            .send(MonitorCommand::ReadHistory {
                request_id: "history-2".into(),
                window_ms: 60_000,
            })
            .unwrap();
        match receiver.blocking_recv() {
            Some(MonitorEvent::HistoryChunk { request_id, done, snapshots }) => {
                assert_eq!(request_id, "history-2");
                assert!(done);
                assert_eq!(snapshots.len(), 1);
                assert_eq!(snapshots[0].request_id, None);
            }
            other => panic!("unexpected {other:?}"),
        }
        handle.send(MonitorCommand::ProcessTable { request_id: "table-1".into() }).unwrap();
        match receiver.blocking_recv() {
            Some(MonitorEvent::ProcessTable { processes, .. }) => {
                assert!(processes.iter().all(|p| p.pid > 0));
                assert!(processes.windows(2).all(|w| w[0].pid < w[1].pid));
            }
            other => panic!("unexpected {other:?}"),
        }
        handle.send(MonitorCommand::Shutdown).unwrap();
        assert!(receiver.blocking_recv().is_none());
    }
}
