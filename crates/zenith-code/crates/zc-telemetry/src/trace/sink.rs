//! `makeTraceSink` and the shared `RotatingFileSink`: trace records buffered as NDJSON lines,
//! flushed every batch window (or at 256 buffered records) into `server.trace.ndjson`, which
//! rotates to `.1` … `.N` (`T3CODE_TRACE_MAX_FILES` backups of `T3CODE_TRACE_MAX_BYTES` each,
//! 10 × 10 MiB by default). A record larger than a whole file is dropped; a failed write keeps
//! the unwritten records for the next flush.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const FLUSH_BUFFER_THRESHOLD: usize = 256;

/// Port of `RotatingFileSink` with `throwOnError: true`.
#[derive(Debug)]
pub struct RotatingFileSink {
    file_path: PathBuf,
    max_bytes: u64,
    max_files: u64,
    current_size: u64,
}

impl RotatingFileSink {
    pub fn new(file_path: impl Into<PathBuf>, max_bytes: u64, max_files: u64) -> std::io::Result<Self> {
        if max_bytes < 1 || max_files < 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "maxBytes and maxFiles must be at least 1",
            ));
        }
        let file_path = file_path.into();
        if let Some(parent) = file_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        let mut sink = Self {
            file_path,
            max_bytes,
            max_files,
            current_size: 0,
        };
        sink.prune_overflow_backups()?;
        sink.current_size = sink.read_current_size()?;
        Ok(sink)
    }

    pub fn file_path(&self) -> &Path {
        &self.file_path
    }

    pub fn write(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }
        if self.current_size > 0 && self.current_size + chunk.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(&self.file_path)?;
        file.write_all(chunk)?;
        self.current_size += chunk.len() as u64;
        Ok(())
    }

    fn with_suffix(&self, index: u64) -> PathBuf {
        let mut name = self.file_path.as_os_str().to_owned();
        name.push(format!(".{index}"));
        PathBuf::from(name)
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        let oldest = self.with_suffix(self.max_files);
        if oldest.exists() {
            fs::remove_file(&oldest)?;
        }
        for index in (1..self.max_files).rev() {
            let source = self.with_suffix(index);
            if source.exists() {
                fs::rename(&source, self.with_suffix(index + 1))?;
            }
        }
        if self.file_path.exists() {
            fs::rename(&self.file_path, self.with_suffix(1))?;
        }
        self.current_size = 0;
        Ok(())
    }

    fn prune_overflow_backups(&self) -> std::io::Result<()> {
        let Some(dir) = self.file_path.parent() else {
            return Ok(());
        };
        let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
        let Some(base) = self.file_path.file_name().and_then(|n| n.to_str()) else {
            return Ok(());
        };
        let prefix = format!("{base}.");
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(suffix) = name.strip_prefix(&prefix) else { continue };
            match suffix.parse::<u64>() {
                Ok(index) if index > self.max_files => {
                    let _ = fs::remove_file(entry.path());
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn read_current_size(&self) -> std::io::Result<u64> {
        match fs::metadata(&self.file_path) {
            Ok(metadata) => Ok(metadata.len()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(error) => Err(error),
        }
    }
}

/// What one flush wrote (`TraceSinkFlushStats`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FlushStats {
    pub logical_write_bytes: u64,
    pub count: u64,
    pub duration_ms: f64,
}

pub type OnFlush = Box<dyn Fn(FlushStats) + Send + Sync>;

pub struct TraceSinkOptions {
    pub file_path: PathBuf,
    pub max_bytes: u64,
    pub max_files: u64,
    pub batch_window: Duration,
    pub on_flush: Option<OnFlush>,
}

struct SinkState {
    buffer: Vec<String>,
    file: RotatingFileSink,
    pending: FlushStats,
}

struct SinkInner {
    state: Mutex<SinkState>,
    max_bytes: u64,
    on_flush: Option<OnFlush>,
    closed: AtomicBool,
    wake: Condvar,
    wake_lock: Mutex<()>,
}

/// `TraceSink`: cheap to clone; a background thread flushes every batch window until
/// [`TraceSink::close`].
#[derive(Clone)]
pub struct TraceSink {
    inner: Arc<SinkInner>,
    file_path: PathBuf,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

impl SinkInner {
    fn flush_unsafe(&self, state: &mut SinkState) {
        if state.buffer.is_empty() {
            return;
        }
        let records = std::mem::take(&mut state.buffer);
        let mut persisted = 0;
        while persisted < records.len() {
            let first_bytes = records[persisted].len() as u64;
            if first_bytes > self.max_bytes {
                persisted += 1;
                continue;
            }
            let mut next = persisted + 1;
            let mut chunk_bytes = first_bytes;
            while next < records.len() {
                let bytes = records[next].len() as u64;
                if chunk_bytes + bytes > self.max_bytes {
                    break;
                }
                chunk_bytes += bytes;
                next += 1;
            }
            let chunk = records[persisted..next].concat();
            let started = Instant::now();
            if state.file.write(chunk.as_bytes()).is_err() {
                let mut kept = records[persisted..].to_vec();
                kept.append(&mut state.buffer);
                state.buffer = kept;
                return;
            }
            state.pending.logical_write_bytes += chunk_bytes;
            state.pending.count += (next - persisted) as u64;
            state.pending.duration_ms += started.elapsed().as_secs_f64() * 1_000.0;
            persisted = next;
        }
    }

    fn flush(&self) {
        let stats = {
            let mut state = lock(&self.state);
            self.flush_unsafe(&mut state);
            std::mem::take(&mut state.pending)
        };
        if stats.count > 0 {
            if let Some(on_flush) = &self.on_flush {
                on_flush(stats);
            }
        }
    }
}

impl TraceSink {
    /// Opens the file (creating its directory, pruning backups beyond `max_files`) and starts
    /// the flush thread.
    pub fn open(options: TraceSinkOptions) -> std::io::Result<Self> {
        let file = RotatingFileSink::new(&options.file_path, options.max_bytes, options.max_files)?;
        let inner = Arc::new(SinkInner {
            state: Mutex::new(SinkState {
                buffer: Vec::new(),
                file,
                pending: FlushStats::default(),
            }),
            max_bytes: options.max_bytes,
            on_flush: options.on_flush,
            closed: AtomicBool::new(false),
            wake: Condvar::new(),
            wake_lock: Mutex::new(()),
        });
        let weak = Arc::downgrade(&inner);
        let window = options.batch_window.max(Duration::from_millis(1));
        std::thread::Builder::new().name("zenith-trace-sink".into()).spawn(move || loop {
            let Some(inner) = weak.upgrade() else { return };
            let guard = lock(&inner.wake_lock);
            let _ = inner.wake.wait_timeout(guard, window);
            if inner.closed.load(Ordering::Acquire) {
                return;
            }
            inner.flush();
        })?;
        Ok(Self {
            inner,
            file_path: options.file_path,
        })
    }

    pub fn file_path(&self) -> &Path {
        &self.file_path
    }

    /// Buffers one record (flushing at 256 buffered records).
    pub fn push(&self, record: &serde_json::Value) {
        if self.inner.closed.load(Ordering::Acquire) {
            return;
        }
        let Ok(mut line) = serde_json::to_string(record) else {
            return;
        };
        line.push('\n');
        let mut state = lock(&self.inner.state);
        state.buffer.push(line);
        if state.buffer.len() >= FLUSH_BUFFER_THRESHOLD {
            self.inner.flush_unsafe(&mut state);
        }
    }

    /// Writes everything buffered and reports it.
    pub fn flush(&self) {
        self.inner.flush();
    }

    /// Flushes, then stops the flush thread; later pushes are dropped.
    pub fn close(&self) {
        self.inner.flush();
        self.inner.closed.store(true, Ordering::Release);
        self.inner.wake.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(name: &str, payload: &str) -> serde_json::Value {
        json!({"type": "effect-span", "name": name, "traceId": "t", "spanId": "s", "attributes": {"payload": payload}})
    }

    fn open(dir: &Path, max_bytes: u64, on_flush: Option<OnFlush>) -> TraceSink {
        TraceSink::open(TraceSinkOptions {
            file_path: dir.join("shared.trace.ndjson"),
            max_bytes,
            max_files: 2,
            batch_window: Duration::from_secs(10),
            on_flush,
        })
        .unwrap()
    }

    fn names(path: &Path) -> Vec<String> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["name"].as_str().unwrap().to_owned())
            .collect()
    }

    fn trace_files(dir: &Path) -> Vec<String> {
        let mut files: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n == "shared.trace.ndjson" || n.starts_with("shared.trace.ndjson."))
            .collect();
        files.sort();
        files
    }

    #[test]
    fn flushes_buffered_trace_records_on_close() {
        let dir = tempfile::tempdir().unwrap();
        let sink = open(dir.path(), 1024, None);
        sink.push(&record("alpha", ""));
        sink.push(&record("beta", ""));
        sink.close();
        assert_eq!(names(&dir.path().join("shared.trace.ndjson")), ["alpha", "beta"]);
    }

    #[test]
    fn reports_successful_logical_trace_writes() {
        let dir = tempfile::tempdir().unwrap();
        let reported = Arc::new(Mutex::new(Vec::new()));
        let seen = reported.clone();
        let sink = open(dir.path(), 1024, Some(Box::new(move |stats| seen.lock().unwrap().push(stats))));
        sink.push(&record("attributed", ""));
        sink.flush();
        let stats = reported.lock().unwrap().clone();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].count, 1);
        assert!(stats[0].logical_write_bytes > 0);
    }

    #[test]
    fn rotates_the_trace_file_when_the_configured_max_size_is_exceeded() {
        let dir = tempfile::tempdir().unwrap();
        let sink = open(dir.path(), 500, None);
        for index in 0..8 {
            sink.push(&record("rotate", &format!("{index}-{}", "x".repeat(48))));
            sink.flush();
        }
        sink.close();
        let files = trace_files(dir.path());
        assert!(files.contains(&"shared.trace.ndjson.1".to_owned()));
        assert!(!files.contains(&"shared.trace.ndjson.3".to_owned()));
    }

    #[test]
    fn keeps_every_trace_file_within_the_configured_limit_for_threshold_flushes() {
        let dir = tempfile::tempdir().unwrap();
        let sink = open(dir.path(), 1_024, None);
        for index in 0..256 {
            sink.push(&record("threshold", &format!("{index}-{}", "x".repeat(48))));
        }
        sink.close();
        let files = trace_files(dir.path());
        assert!(files.contains(&"shared.trace.ndjson.1".to_owned()));
        for file in files {
            assert!(fs::metadata(dir.path().join(&file)).unwrap().len() <= 1_024, "{file}");
        }
    }

    #[test]
    fn drops_a_single_trace_record_that_cannot_fit_within_the_configured_limit() {
        let dir = tempfile::tempdir().unwrap();
        let sink = open(dir.path(), 1_024, None);
        sink.push(&record("oversized", &"x".repeat(2_048)));
        sink.push(&record("retained", ""));
        sink.close();
        let path = dir.path().join("shared.trace.ndjson");
        assert_eq!(names(&path), ["retained"]);
        assert!(fs::metadata(&path).unwrap().len() <= 1_024);
    }

    #[test]
    fn prunes_backups_beyond_max_files_and_resumes_at_the_current_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.trace.ndjson");
        fs::write(&path, "x\n").unwrap();
        fs::write(dir.path().join("shared.trace.ndjson.5"), "old\n").unwrap();
        fs::write(dir.path().join("shared.trace.ndjson.2"), "kept\n").unwrap();
        let sink = RotatingFileSink::new(&path, 10, 2).unwrap();
        assert_eq!(sink.current_size, 2);
        assert_eq!(trace_files(dir.path()), ["shared.trace.ndjson", "shared.trace.ndjson.2"]);
    }
}
