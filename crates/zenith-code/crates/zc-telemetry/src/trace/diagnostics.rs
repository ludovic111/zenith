//! `diagnostics/TraceDiagnostics.ts` (`server.getTraceDiagnostics`): folds the trace NDJSON and
//! its rotated backups, oldest first, into span counts, the slowest spans, failures and the
//! latest warning and error logs. Files are streamed line by line.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use zc_contracts::{
    DateTimeUtc, EOption, JsNumber, ServerTraceDiagnosticsErrorKind, ServerTraceDiagnosticsFailureSummary, ServerTraceDiagnosticsLogEvent,
    ServerTraceDiagnosticsRecentFailure, ServerTraceDiagnosticsResult, ServerTraceDiagnosticsResultErrorValue, ServerTraceDiagnosticsSpanOccurrence,
    ServerTraceDiagnosticsSpanSummary,
};

pub const DEFAULT_SLOW_SPAN_THRESHOLD_MS: i64 = 1_000;
const TOP_LIMIT: usize = 10;
const RECENT_LIMIT: usize = 20;

/// `toRotatedTracePaths`: the backups (`.N` … `.1`) then the file itself.
pub fn rotated_trace_paths(trace_file_path: &Path, max_files: i64) -> Vec<PathBuf> {
    let backups = max_files.max(0);
    let mut paths: Vec<PathBuf> = (0..backups)
        .map(|index| {
            let mut name = trace_file_path.as_os_str().to_owned();
            name.push(format!(".{}", backups - index));
            PathBuf::from(name)
        })
        .collect();
    paths.push(trace_file_path.to_path_buf());
    paths
}

fn string_value(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str).filter(|s| !s.trim().is_empty())
}

fn number_value(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|v| v.is_finite())
}

/// `unixNanoToDateTime`: a decimal nanosecond string to a millisecond date.
fn unix_nano_to_date(value: Option<&Value>) -> Option<DateTimeUtc> {
    let text = string_value(value)?.trim();
    let nanos: i128 = text.parse().ok()?;
    let millis = i64::try_from(nanos / 1_000_000).ok()?;
    DateTimeUtc::from_millis(millis).ok()
}

/// Inserts into a list kept sorted by `order` with at most `limit` entries (stable).
fn insert_bounded<T>(items: &mut Vec<T>, item: T, limit: usize, order: impl Fn(&T, &T) -> std::cmp::Ordering) {
    if items.len() >= limit {
        if let Some(last) = items.last() {
            if order(&item, last) != std::cmp::Ordering::Less {
                return;
            }
        }
    }
    let position = items.partition_point(|existing| order(existing, &item) != std::cmp::Ordering::Greater);
    items.insert(position, item);
    items.truncate(limit);
}

fn by_desc_f64(a: f64, b: f64) -> std::cmp::Ordering {
    b.partial_cmp(&a).unwrap_or(std::cmp::Ordering::Equal)
}

#[derive(Default)]
struct SpanStats {
    count: i64,
    failure_count: i64,
    total_duration_ms: f64,
    max_duration_ms: f64,
}

/// The NDJSON fold (`makeTraceDiagnosticsAggregator`).
pub struct TraceDiagnosticsAggregator {
    slow_span_threshold_ms: i64,
    parse_error_count: i64,
    record_count: i64,
    failure_count: i64,
    interruption_count: i64,
    slow_span_count: i64,
    first_span_at: Option<DateTimeUtc>,
    last_span_at: Option<DateTimeUtc>,
    span_order: Vec<String>,
    spans_by_name: HashMap<String, SpanStats>,
    failure_order: Vec<(String, String)>,
    failures_by_key: HashMap<(String, String), ServerTraceDiagnosticsFailureSummary>,
    latest_failures: Vec<ServerTraceDiagnosticsRecentFailure>,
    slowest_spans: Vec<ServerTraceDiagnosticsSpanOccurrence>,
    latest_logs: Vec<ServerTraceDiagnosticsLogEvent>,
    log_level_counts: BTreeMap<String, i64>,
}

impl Default for TraceDiagnosticsAggregator {
    fn default() -> Self {
        Self::new(DEFAULT_SLOW_SPAN_THRESHOLD_MS)
    }
}

impl TraceDiagnosticsAggregator {
    pub fn new(slow_span_threshold_ms: i64) -> Self {
        Self {
            slow_span_threshold_ms,
            parse_error_count: 0,
            record_count: 0,
            failure_count: 0,
            interruption_count: 0,
            slow_span_count: 0,
            first_span_at: None,
            last_span_at: None,
            span_order: Vec::new(),
            spans_by_name: HashMap::new(),
            failure_order: Vec::new(),
            failures_by_key: HashMap::new(),
            latest_failures: Vec::new(),
            slowest_spans: Vec::new(),
            latest_logs: Vec::new(),
            log_level_counts: BTreeMap::new(),
        }
    }

    /// One line of a trace file.
    pub fn add_line(&mut self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        let Ok(parsed) = serde_json::from_str::<Value>(line) else {
            self.parse_error_count += 1;
            return;
        };
        if !parsed.is_object() {
            self.parse_error_count += 1;
            return;
        }
        let name = string_value(parsed.get("name"));
        let trace_id = string_value(parsed.get("traceId"));
        let span_id = string_value(parsed.get("spanId"));
        let duration_ms = number_value(parsed.get("durationMs"));
        let ended_at = unix_nano_to_date(parsed.get("endTimeUnixNano"));
        let started_at = unix_nano_to_date(parsed.get("startTimeUnixNano"));
        let (Some(name), Some(trace_id), Some(span_id), Some(duration_ms), Some(ended_at)) = (name, trace_id, span_id, duration_ms, ended_at) else {
            self.parse_error_count += 1;
            return;
        };
        let (name, trace_id, span_id) = (name.to_owned(), trace_id.to_owned(), span_id.to_owned());

        self.record_count += 1;
        if let Some(started_at) = started_at {
            if self.first_span_at.is_none_or(|first| started_at < first) {
                self.first_span_at = Some(started_at);
            }
        }
        if self.last_span_at.is_none_or(|last| ended_at > last) {
            self.last_span_at = Some(ended_at);
        }

        let exit = parsed.get("exit").filter(|e| e.is_object());
        let exit_tag = exit.and_then(|e| string_value(e.get("_tag")));
        let is_failure = exit_tag == Some("Failure");
        if is_failure {
            self.failure_count += 1;
        }
        if exit_tag == Some("Interrupted") {
            self.interruption_count += 1;
        }

        if !self.spans_by_name.contains_key(&name) {
            self.span_order.push(name.clone());
        }
        let stats = self.spans_by_name.entry(name.clone()).or_default();
        stats.count += 1;
        stats.total_duration_ms += duration_ms;
        stats.max_duration_ms = stats.max_duration_ms.max(duration_ms);
        if is_failure {
            stats.failure_count += 1;
        }

        if duration_ms >= self.slow_span_threshold_ms as f64 {
            self.slow_span_count += 1;
        }
        insert_bounded(
            &mut self.slowest_spans,
            ServerTraceDiagnosticsSpanOccurrence {
                name: name.clone(),
                duration_ms: JsNumber(duration_ms),
                ended_at,
                trace_id: trace_id.clone(),
                span_id: span_id.clone(),
            },
            TOP_LIMIT,
            |a, b| by_desc_f64(a.duration_ms.0, b.duration_ms.0),
        );

        if is_failure {
            let cause = exit
                .and_then(|e| string_value(e.get("cause")))
                .map(|c| c.trim().to_owned())
                .unwrap_or_else(|| "Failure".to_owned());
            insert_bounded(
                &mut self.latest_failures,
                ServerTraceDiagnosticsRecentFailure {
                    name: name.clone(),
                    cause: cause.clone(),
                    duration_ms: JsNumber(duration_ms),
                    ended_at,
                    trace_id: trace_id.clone(),
                    span_id: span_id.clone(),
                },
                RECENT_LIMIT,
                |a, b| b.ended_at.cmp(&a.ended_at),
            );
            let key = (name.clone(), cause.clone());
            match self.failures_by_key.get_mut(&key) {
                Some(existing) => {
                    existing.count += 1;
                    if ended_at > existing.last_seen_at {
                        existing.last_seen_at = ended_at;
                        existing.trace_id = trace_id.clone();
                        existing.span_id = span_id.clone();
                    }
                }
                None => {
                    self.failure_order.push(key.clone());
                    self.failures_by_key.insert(
                        key,
                        ServerTraceDiagnosticsFailureSummary {
                            name: name.clone(),
                            cause,
                            count: 1,
                            last_seen_at: ended_at,
                            trace_id: trace_id.clone(),
                            span_id: span_id.clone(),
                        },
                    );
                }
            }
        }

        if let Some(events) = parsed.get("events").and_then(Value::as_array) {
            for event in events.iter().filter(|e| e.is_object()) {
                let Some(level) = event
                    .get("attributes")
                    .filter(|a| a.is_object())
                    .and_then(|a| string_value(a.get("effect.logLevel")))
                else {
                    continue;
                };
                *self.log_level_counts.entry(level.to_owned()).or_insert(0) += 1;
                let normalized = level.to_lowercase();
                if !matches!(normalized.as_str(), "warning" | "warn" | "error" | "fatal") {
                    continue;
                }
                let seen_at = unix_nano_to_date(event.get("timeUnixNano")).unwrap_or(ended_at);
                let message = string_value(event.get("name"))
                    .map(|m| m.trim().to_owned())
                    .unwrap_or_else(|| "Log event".to_owned());
                insert_bounded(
                    &mut self.latest_logs,
                    ServerTraceDiagnosticsLogEvent {
                        span_name: name.clone(),
                        level: level.to_owned(),
                        message,
                        seen_at,
                        trace_id: trace_id.clone(),
                        span_id: span_id.clone(),
                    },
                    RECENT_LIMIT,
                    |a, b| b.seen_at.cmp(&a.seen_at),
                );
            }
        }
    }

    /// The result (`finish`).
    pub fn finish(
        self,
        trace_file_path: &str,
        scanned_file_paths: Vec<String>,
        read_at: DateTimeUtc,
        error: Option<ServerTraceDiagnosticsResultErrorValue>,
        partial_failure: bool,
    ) -> ServerTraceDiagnosticsResult {
        let mut top: Vec<ServerTraceDiagnosticsSpanSummary> = self
            .span_order
            .iter()
            .filter_map(|name| {
                let stats = self.spans_by_name.get(name)?;
                Some(ServerTraceDiagnosticsSpanSummary {
                    name: name.clone(),
                    count: stats.count,
                    failure_count: stats.failure_count,
                    total_duration_ms: JsNumber(stats.total_duration_ms),
                    average_duration_ms: JsNumber(if stats.count > 0 { stats.total_duration_ms / stats.count as f64 } else { 0.0 }),
                    max_duration_ms: JsNumber(stats.max_duration_ms),
                })
            })
            .collect();
        top.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| by_desc_f64(a.max_duration_ms.0, b.max_duration_ms.0)));
        top.truncate(TOP_LIMIT);
        let mut failures_by_key = self.failures_by_key;
        let mut common: Vec<ServerTraceDiagnosticsFailureSummary> = self.failure_order.iter().filter_map(|key| failures_by_key.remove(key)).collect();
        common.sort_by(|a, b| b.count.cmp(&a.count).then(b.last_seen_at.cmp(&a.last_seen_at)));
        common.truncate(TOP_LIMIT);
        ServerTraceDiagnosticsResult {
            trace_file_path: trace_file_path.to_owned(),
            scanned_file_paths,
            read_at,
            record_count: self.record_count,
            parse_error_count: self.parse_error_count,
            first_span_at: EOption(self.first_span_at),
            last_span_at: EOption(self.last_span_at),
            failure_count: self.failure_count,
            interruption_count: self.interruption_count,
            slow_span_threshold_ms: self.slow_span_threshold_ms,
            slow_span_count: self.slow_span_count,
            log_level_counts: self.log_level_counts,
            top_spans_by_count: top,
            slowest_spans: self.slowest_spans,
            common_failures: common,
            latest_failures: self.latest_failures,
            latest_warning_and_error_logs: self.latest_logs,
            partial_failure: EOption(partial_failure.then_some(true)),
            error: EOption(error),
        }
    }
}

/// Feeds each line of one file to `on_line`; `Ok(false)` when the file does not exist.
pub fn stream_trace_file_lines(path: &Path, mut on_line: impl FnMut(&str)) -> std::io::Result<bool> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        if reader.read_until(b'\n', &mut buffer)? == 0 {
            return Ok(true);
        }
        if buffer.last() == Some(&b'\n') {
            buffer.pop();
        }
        if buffer.last() == Some(&b'\r') {
            buffer.pop();
        }
        on_line(&String::from_utf8_lossy(&buffer));
    }
}

/// `TraceDiagnostics.read`.
pub fn read_trace_diagnostics(
    trace_file_path: &Path,
    max_files: i64,
    slow_span_threshold_ms: Option<i64>,
    read_at: DateTimeUtc,
) -> ServerTraceDiagnosticsResult {
    let threshold = slow_span_threshold_ms.unwrap_or(DEFAULT_SLOW_SPAN_THRESHOLD_MS);
    let paths = rotated_trace_paths(trace_file_path, max_files);
    let scanned: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    let mut aggregator = TraceDiagnosticsAggregator::new(threshold);
    let mut found = false;
    let mut read_failure: Option<String> = None;
    for path in &paths {
        match stream_trace_file_lines(path, |line| aggregator.add_line(line)) {
            Ok(exists) => found |= exists,
            Err(error) => {
                tracing::warn!(
                    traceFilePath = %path.display(),
                    errorTag = "TraceFileReadError",
                    causeTag = ?error.kind(),
                    "Failed to read local trace file."
                );
                if read_failure.is_none() {
                    read_failure = Some(format!("Failed to read local trace file '{}'.", path.display()));
                }
            }
        }
    }
    let trace_file_path = trace_file_path.to_string_lossy();
    let read_failure = read_failure.map(|message| ServerTraceDiagnosticsResultErrorValue {
        kind: ServerTraceDiagnosticsErrorKind::TraceFileReadFailed,
        message,
    });
    if !found {
        let error = read_failure.unwrap_or_else(|| ServerTraceDiagnosticsResultErrorValue {
            kind: ServerTraceDiagnosticsErrorKind::TraceFileNotFound,
            message: "No local trace files were found.".to_owned(),
        });
        return TraceDiagnosticsAggregator::new(threshold).finish(&trace_file_path, scanned, read_at, Some(error), false);
    }
    let partial = read_failure.is_some();
    aggregator.finish(&trace_file_path, scanned, read_at, read_failure, partial)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ns(ms: i64) -> String {
        (i128::from(ms) * 1_000_000).to_string()
    }

    fn record(name: &str, trace_id: &str, span_id: &str, start_ms: i64, duration_ms: i64, exit: Option<Value>, events: Vec<Value>) -> String {
        json!({
            "type": "effect-span",
            "name": name,
            "traceId": trace_id,
            "spanId": span_id,
            "sampled": true,
            "kind": "internal",
            "startTimeUnixNano": ns(start_ms),
            "endTimeUnixNano": ns(start_ms + duration_ms),
            "durationMs": duration_ms,
            "attributes": {},
            "events": events,
            "links": [],
            "exit": exit.unwrap_or_else(|| json!({"_tag": "Success"})),
        })
        .to_string()
    }

    fn log(name: &str, at_ms: i64, level: &str) -> Value {
        json!({"name": name, "timeUnixNano": ns(at_ms), "attributes": {"effect.logLevel": level}})
    }

    const TRACE_FILE: &str = "/tmp/server.trace.ndjson";

    fn read_at() -> DateTimeUtc {
        DateTimeUtc::parse("2026-05-05T10:00:00.000Z").unwrap()
    }

    fn aggregate_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> ServerTraceDiagnosticsResult {
        let mut aggregator = TraceDiagnosticsAggregator::default();
        for line in lines {
            aggregator.add_line(line);
        }
        let scanned = rotated_trace_paths(Path::new(TRACE_FILE), 1)
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        aggregator.finish(TRACE_FILE, scanned, read_at(), None, false)
    }

    #[test]
    fn aggregates_failures_slow_spans_log_levels_and_parse_errors() {
        let failure = || Some(json!({"_tag": "Failure", "cause": "Provider crashed"}));
        let lines = [
            record("server.getConfig", "trace-a", "span-a", 1_000, 50, None, vec![]),
            "not-json".to_owned(),
            record(
                "orchestration.dispatch",
                "trace-b",
                "span-b",
                2_000,
                1_500,
                failure(),
                vec![log("provider failed", 3_400, "Error")],
            ),
            record("orchestration.dispatch", "trace-c", "span-c", 4_000, 250, failure(), vec![]),
            record(
                "git.status",
                "trace-d",
                "span-d",
                5_000,
                25,
                Some(json!({"_tag": "Interrupted", "cause": "Interrupted"})),
                vec![log("status delayed", 5_010, "Warning")],
            ),
        ];
        let d = aggregate_lines(lines.iter().map(String::as_str));
        assert_eq!(d.record_count, 4);
        assert_eq!(d.read_at.to_iso_string(), "2026-05-05T10:00:00.000Z");
        assert_eq!(d.first_span_at.0.unwrap().to_iso_string(), "1970-01-01T00:00:01.000Z");
        assert_eq!(d.last_span_at.0.unwrap().to_iso_string(), "1970-01-01T00:00:05.025Z");
        assert_eq!(d.parse_error_count, 1);
        assert_eq!(d.failure_count, 2);
        assert_eq!(d.interruption_count, 1);
        assert_eq!(d.slow_span_count, 1);
        assert_eq!(d.log_level_counts["Error"], 1);
        assert_eq!(d.log_level_counts["Warning"], 1);
        assert_eq!(d.common_failures[0].name, "orchestration.dispatch");
        assert_eq!(d.common_failures[0].count, 2);
        assert_eq!(d.latest_failures[0].trace_id, "trace-c");
        assert_eq!(d.slowest_spans[0].trace_id, "trace-b");
        assert_eq!(d.latest_warning_and_error_logs[0].message, "status delayed");
        assert_eq!(d.top_spans_by_count[0].name, "orchestration.dispatch");
    }

    #[test]
    fn returns_a_not_found_diagnostic_when_no_files_are_available() {
        let dir = tempfile::tempdir().unwrap();
        let d = read_trace_diagnostics(&dir.path().join("server.trace.ndjson"), 1, None, read_at());
        assert_eq!(d.record_count, 0);
        assert_eq!(d.error.0.unwrap().kind, ServerTraceDiagnosticsErrorKind::TraceFileNotFound);
        assert_eq!(d.partial_failure.0, None);
    }

    #[test]
    fn streams_rotated_files_into_the_same_result_as_reading_them_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.trace.ndjson");
        let failure = || Some(json!({"_tag": "Failure", "cause": "Provider crashed: café 🔥"}));
        let older = [
            record("server.getConfig", "trace-a", "span-a", 1_000, 50, None, vec![]),
            "not-json".to_owned(),
            record("orchestration.dispatch", "trace-b", "span-b", 2_000, 1_500, failure(), vec![]),
            String::new(),
        ]
        .join("\r\n");
        let newer = [
            record(
                "git.status",
                "trace-c",
                "span-c",
                3_000,
                25,
                Some(json!({"_tag": "Interrupted", "cause": "Interrupted"})),
                vec![log("status delayed ⏳", 3_010, "Warning")],
            ),
            String::new(),
            record("orchestration.dispatch", "trace-d", "span-d", 4_000, 250, failure(), vec![]),
        ]
        .join("\n");
        std::fs::write(dir.path().join("server.trace.ndjson.1"), &older).unwrap();
        std::fs::write(&path, &newer).unwrap();
        let streamed = read_trace_diagnostics(&path, 1, None, read_at());
        assert_eq!(streamed.record_count, 4);
        let mut whole = TraceDiagnosticsAggregator::default();
        for text in [&older, &newer] {
            for line in text.split('\n') {
                whole.add_line(line.strip_suffix('\r').unwrap_or(line));
            }
        }
        let scanned = rotated_trace_paths(&path, 1).iter().map(|p| p.to_string_lossy().into_owned()).collect();
        assert_eq!(streamed, whole.finish(&path.to_string_lossy(), scanned, read_at(), None, false));
    }

    #[test]
    fn preserves_full_failure_causes_and_log_messages() {
        let long_cause = format!("VcsProcessSpawnError: {}", "missing executable ".repeat(80)).trim().to_owned();
        let long_message = format!("provider warning: {}", "retrying command ".repeat(80)).trim().to_owned();
        let line = record(
            "VcsProcess.run",
            "trace-long",
            "span-long",
            1_000,
            25,
            Some(json!({"_tag": "Failure", "cause": long_cause})),
            vec![log(&long_message, 1_010, "Warning")],
        );
        let d = aggregate_lines([line.as_str()]);
        assert_eq!(d.latest_failures[0].cause, long_cause);
        assert_eq!(d.common_failures[0].cause, long_cause);
        assert_eq!(d.latest_warning_and_error_logs[0].message, long_message);
    }

    #[cfg(unix)]
    #[test]
    fn keeps_loaded_trace_data_when_one_rotated_trace_file_fails_to_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.trace.ndjson");
        // A directory where a file is expected fails to read (not "not found").
        std::fs::create_dir(dir.path().join("server.trace.ndjson.1")).unwrap();
        std::fs::write(&path, record("server.getConfig", "trace-a", "span-a", 1_000, 50, None, vec![])).unwrap();
        let d = read_trace_diagnostics(&path, 1, None, read_at());
        assert_eq!(d.record_count, 1);
        assert_eq!(d.partial_failure.0, Some(true));
        let error = d.error.0.unwrap();
        assert_eq!(error.kind, ServerTraceDiagnosticsErrorKind::TraceFileReadFailed);
        assert_eq!(error.message, format!("Failed to read local trace file '{}.1'.", path.display()));
        assert_eq!(d.scanned_file_paths, [format!("{}.1", path.display()), path.display().to_string()]);
    }

    #[test]
    fn keeps_only_the_top_spans_failures_and_warning_logs_from_large_inputs() {
        // Shuffled, so some older records arrive after the lists are full.
        let lines: Vec<String> = (0..30)
            .map(|step| (step * 7) % 30)
            .map(|index| {
                record(
                    &format!("span-{index}"),
                    &format!("trace-{index}"),
                    &format!("span-{index}"),
                    index * 1_000,
                    index,
                    Some(json!({"_tag": "Failure", "cause": "Provider crashed"})),
                    vec![log(&format!("warning {index}"), index * 1_000, "Warning")],
                )
            })
            .collect();
        let d = aggregate_lines(lines.iter().map(String::as_str));
        let newest: Vec<String> = (0..20).map(|rank| format!("trace-{}", 29 - rank)).collect();
        assert_eq!(d.record_count, 30);
        assert_eq!(
            d.slowest_spans.iter().map(|s| s.duration_ms.0 as i64).collect::<Vec<_>>(),
            [29, 28, 27, 26, 25, 24, 23, 22, 21, 20]
        );
        assert_eq!(d.latest_failures.iter().map(|f| f.trace_id.clone()).collect::<Vec<_>>(), newest);
        assert_eq!(d.latest_warning_and_error_logs.iter().map(|l| l.trace_id.clone()).collect::<Vec<_>>(), newest);
    }
}
