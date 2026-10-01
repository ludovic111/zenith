//! `diagnostics/ProcessDiagnostics.ts` (`server.getProcessDiagnostics`, `server.signalProcess`)
//! and `diagnostics/ProcessResourceMonitor.ts` (`server.getProcessResourceHistory`): the
//! legacy Diagnostics contracts projected from resource telemetry.

use zc_contracts::{
    EOption, JsNumber, ResourceTelemetryProcessCategory, ServerProcessDiagnosticsEntry, ServerProcessDiagnosticsResult,
    ServerProcessDiagnosticsResultErrorValue, ServerProcessResourceHistoryBucket, ServerProcessResourceHistoryFailureTag, ServerProcessResourceHistoryResult,
    ServerProcessResourceHistoryResultErrorValue, ServerProcessResourceHistorySummary, ServerProcessSignal, ServerSignalProcessInput,
    ServerSignalProcessResult,
};

use crate::history::HistoryWithLegacyBuckets;
use crate::model::is_backend_category;
use crate::ResourceTelemetry;

/// `formatElapsed`: `m:ss`, or `h:mm:ss` from one hour.
pub fn format_elapsed(run_time_ms: i64) -> String {
    let total_seconds = (run_time_ms / 1_000).max(0);
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Server descendants the Diagnostics page may signal (never the server or Electron).
pub fn can_signal_category(category: ResourceTelemetryProcessCategory) -> bool {
    use ResourceTelemetryProcessCategory as C;
    matches!(category, C::ServerChild | C::ProviderRoot | C::TerminalRoot)
}

fn non_empty(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

/// `ProcessDiagnostics.read`: the server's descendants from a fresh sample (or the latest).
pub async fn read_process_diagnostics(telemetry: &ResourceTelemetry) -> ServerProcessDiagnosticsResult {
    let snapshot = telemetry.refreshed_or_latest().await;
    let processes: Vec<ServerProcessDiagnosticsEntry> = snapshot
        .processes
        .iter()
        .filter(|entry| can_signal_category(entry.category))
        .map(|entry| ServerProcessDiagnosticsEntry {
            pid: entry.identity.pid,
            start_time_ms: entry.identity.start_time_ms,
            ppid: entry.ppid,
            pgid: EOption(None),
            status: non_empty(&entry.status).unwrap_or("Unknown").to_owned(),
            cpu_percent: entry.cpu_percent,
            rss_bytes: entry.resident_bytes,
            elapsed: format_elapsed(entry.run_time_ms),
            command: non_empty(&entry.command).or(non_empty(&entry.name)).unwrap_or("unknown").to_owned(),
            depth: (entry.depth - 1).max(0),
            child_pids: entry.child_pids.clone(),
        })
        .collect();
    ServerProcessDiagnosticsResult {
        server_pid: telemetry.server_pid(),
        read_at: snapshot.read_at,
        process_count: processes.len() as i64,
        total_rss_bytes: processes.iter().map(|p| p.rss_bytes).sum(),
        total_cpu_percent: JsNumber(processes.iter().map(|p| p.cpu_percent.0).sum()),
        processes,
        error: EOption(
            snapshot
                .health
                .native
                .last_error
                .0
                .map(|message| ServerProcessDiagnosticsResultErrorValue { message }),
        ),
    }
}

fn signal_result(input: &ServerSignalProcessInput, signaled: bool, message: Option<String>) -> ServerSignalProcessResult {
    ServerSignalProcessResult {
        pid: input.pid,
        signal: input.signal,
        signaled,
        message: EOption(message),
    }
}

/// Sends `signal` to `pid` (`process.kill`).
pub type Killer = dyn Fn(i64, ServerProcessSignal) -> std::io::Result<()> + Send + Sync;

/// `kill(2)`.
pub fn kill_process(pid: i64, signal: ServerProcessSignal) -> std::io::Result<()> {
    let pid = libc::pid_t::try_from(pid).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let signal = match signal {
        ServerProcessSignal::SIGINT => libc::SIGINT,
        ServerProcessSignal::SIGKILL => libc::SIGKILL,
    };
    // SAFETY: kill(2) has no memory-safety preconditions.
    if unsafe { libc::kill(pid, signal) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// `ProcessDiagnostics.signal`: only a server descendant whose `(pid, startTimeMs)` identity
/// still matches a fresh sample.
pub async fn signal_process(telemetry: &ResourceTelemetry, input: &ServerSignalProcessInput, kill: &Killer) -> ServerSignalProcessResult {
    let pid = input.pid;
    if pid == telemetry.server_pid() {
        return signal_result(input, false, Some("Refusing to signal the T3 server process.".into()));
    }
    let Ok(current) = telemetry.refresh().await else {
        return signal_result(
            input,
            false,
            Some(format!("Could not refresh process {pid}; refusing to signal a stale identity.")),
        );
    };
    let Some(selected) = current
        .processes
        .iter()
        .find(|entry| entry.identity.pid == pid && entry.identity.start_time_ms == input.start_time_ms)
    else {
        return signal_result(input, false, Some(format!("Process {pid} no longer matches the selected process identity.")));
    };
    if !can_signal_category(selected.category) {
        return signal_result(input, false, Some(format!("Process {pid} is not a signalable T3 backend descendant.")));
    }
    match kill(pid, input.signal) {
        Ok(()) => signal_result(input, true, None),
        Err(_) => signal_result(input, false, Some(format!("Failed to signal process {pid} with {}.", input.signal.as_str()))),
    }
}

/// `ProcessResourceMonitor.readHistory`: the backend part of the resource history.
pub async fn read_process_resource_history(telemetry: &ResourceTelemetry, window_ms: i64, bucket_ms: i64) -> ServerProcessResourceHistoryResult {
    project_process_resource_history(telemetry.read_history(window_ms, bucket_ms).await)
}

/// The legacy projection of a resource history (backend processes and buckets only).
pub fn project_process_resource_history(history: HistoryWithLegacyBuckets) -> ServerProcessResourceHistoryResult {
    let legacy = history.legacy_backend_buckets;
    let history = history.history;
    let top_processes: Vec<ServerProcessResourceHistorySummary> = history
        .top_processes
        .iter()
        .filter(|entry| is_backend_category(entry.category))
        .map(|entry| ServerProcessResourceHistorySummary {
            process_key: format!("{}:{}", entry.identity.pid, entry.identity.start_time_ms),
            pid: entry.identity.pid,
            ppid: entry.ppid,
            command: non_empty(&entry.command).or(non_empty(&entry.name)).unwrap_or("unknown").to_owned(),
            depth: entry.depth,
            is_server_root: entry.category == ResourceTelemetryProcessCategory::Server,
            first_seen_at: entry.first_seen_at,
            last_seen_at: entry.last_seen_at,
            current_cpu_percent: entry.current_cpu_percent,
            avg_cpu_percent: entry.avg_cpu_percent,
            max_cpu_percent: entry.max_cpu_percent,
            cpu_seconds_approx: JsNumber(entry.cpu_time_ms as f64 / 1_000.0),
            current_rss_bytes: entry.current_rss_bytes,
            max_rss_bytes: entry.peak_rss_bytes,
            sample_count: entry.sample_count,
        })
        .collect();
    ServerProcessResourceHistoryResult {
        read_at: history.read_at,
        window_ms: history.window_ms,
        bucket_ms: history.bucket_ms,
        sample_interval_ms: history.sample_interval_ms,
        retained_sample_count: history.retained_sample_count,
        total_cpu_seconds_approx: JsNumber(top_processes.iter().map(|p| p.cpu_seconds_approx.0).sum()),
        buckets: legacy
            .into_iter()
            .map(|bucket| ServerProcessResourceHistoryBucket {
                started_at: bucket.started_at,
                ended_at: bucket.ended_at,
                avg_cpu_percent: bucket.avg_cpu_percent,
                max_cpu_percent: bucket.max_cpu_percent,
                max_rss_bytes: bucket.max_rss_bytes,
                max_process_count: bucket.max_process_count,
            })
            .collect(),
        top_processes,
        error: EOption(history.health.native.last_error.0.map(|message| ServerProcessResourceHistoryResultErrorValue {
            failure_tag: ServerProcessResourceHistoryFailureTag::ProcessDiagnosticsQueryFailedError,
            message,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_elapsed_like_ps() {
        assert_eq!(format_elapsed(4_000), "0:04");
        assert_eq!(format_elapsed(61_999), "1:01");
        assert_eq!(format_elapsed(3_600_000 + 62_000), "1:01:02");
        assert_eq!(format_elapsed(-5), "0:00");
    }
}
