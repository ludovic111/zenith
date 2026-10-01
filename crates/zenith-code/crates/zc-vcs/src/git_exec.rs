//! The git process runner of `GitVcsDriverCore.ts` (`executeRaw` / `execute` /
//! `collectOutput` / `createTrace2Monitor`).
//!
//! This is *not* `VcsProcess` (zc-core): the core driver spawns `git` itself, with its own
//! semantics, kept here exactly:
//! - at most **8** git processes run at once across every driver instance; commands with no
//!   timeout or a timeout above 30 s (push, worktree add/remove) skip the queue. The timeout
//!   starts once a permit is held, so queued commands never time out while waiting;
//! - default timeout 30 s, `GitTimeout::Unbounded` for none → `"Git command timed out."`;
//! - per-stream cap (default 1,000,000 bytes): without `append_truncation_marker` going past it
//!   fails with `"Git output exceeded N bytes and was truncated."` (`outputLength`); with it the
//!   text is cut at the cap and flagged truncated (callers append the marker);
//! - line callbacks split on `\r\n`, `\r` or `\n` (git redraws progress with bare `\r`), skip
//!   empty lines, and optionally keep flowing past the cap;
//! - hook progress through `GIT_TRACE2_EVENT=<temp file>`, tailed while git runs and flushed
//!   at the end (`child_start` / `child_exit` events of `child_class: "hook"`);
//! - stderr never enters an error, only its length.
//!
//! Deviation: a command without `stdin` gets `/dev/null` instead of an idle pipe, so a hook
//! that reads stdin sees EOF instead of hanging until the timeout.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::Semaphore;
use zc_core::defect::{js_length, Defect};

use crate::errors::{platform_error_defect, GitCommandError};

pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1_000_000;
pub const OUTPUT_TRUNCATED_MARKER: &str = "\n\n[truncated]";
const GIT_PROCESS_CONCURRENCY: usize = 8;
const MAX_PENDING_LINE_BYTES: usize = 64 * 1024;
const TRACE_POLL_INTERVAL: Duration = Duration::from_millis(25);
const READ_CHUNK_BYTES: usize = 64 * 1024;
const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

/// Environment overlay: `None` unsets a variable.
pub type EnvOverlay = BTreeMap<String, Option<String>>;

/// Builds an [`EnvOverlay`] from `(key, value)` pairs.
pub fn env<const N: usize>(pairs: [(&str, &str); N]) -> EnvOverlay {
    pairs.into_iter().map(|(k, v)| (k.to_owned(), Some(v.to_owned()))).collect()
}

/// `timeoutMs`: `undefined` (30 s), a number, or `null` (no timeout).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GitTimeout {
    #[default]
    Default,
    Millis(u64),
    Unbounded,
}

impl GitTimeout {
    fn duration(self) -> Option<Duration> {
        match self {
            Self::Default => Some(Duration::from_millis(DEFAULT_TIMEOUT_MS)),
            Self::Millis(ms) => Some(Duration::from_millis(ms)),
            Self::Unbounded => None,
        }
    }

    /// Long commands bypass the shared git queue.
    fn uses_queue(self) -> bool {
        match self {
            Self::Default => true,
            Self::Millis(ms) => ms <= DEFAULT_TIMEOUT_MS,
            Self::Unbounded => false,
        }
    }
}

pub type LineCallback = Arc<dyn Fn(&str) + Send + Sync>;
/// `onHookStarted(hookName)`.
pub type HookStartedCallback = Arc<dyn Fn(&str) + Send + Sync>;
/// `onHookFinished({hookName, exitCode, durationMs})`.
pub type HookFinishedCallback = Arc<dyn Fn(HookFinished) + Send + Sync>;

/// `onHookFinished` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookFinished {
    pub hook_name: String,
    pub exit_code: Option<i64>,
    pub duration_ms: Option<u64>,
}

/// `ExecuteGitProgress`.
#[derive(Clone, Default)]
pub struct ExecuteGitProgress {
    pub on_stdout_line: Option<LineCallback>,
    pub on_stderr_line: Option<LineCallback>,
    pub on_hook_started: Option<HookStartedCallback>,
    pub on_hook_finished: Option<HookFinishedCallback>,
}

/// `ExecuteGitInput`.
#[derive(Clone, Default)]
pub struct ExecuteGitInput {
    pub operation: String,
    pub cwd: String,
    pub args: Vec<String>,
    pub stdin: Option<String>,
    pub env: Option<EnvOverlay>,
    pub allow_non_zero_exit: bool,
    pub timeout: GitTimeout,
    pub max_output_bytes: Option<usize>,
    pub append_truncation_marker: bool,
    /// With `append_truncation_marker`, keep invoking line callbacks after the buffered copy is
    /// full.
    pub keep_line_callbacks_after_truncation: bool,
    pub progress: Option<ExecuteGitProgress>,
}

impl ExecuteGitInput {
    pub fn new<I, S>(operation: &str, cwd: &str, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            operation: operation.to_owned(),
            cwd: cwd.to_owned(),
            args: args.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }
}

impl std::fmt::Debug for ExecuteGitInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecuteGitInput")
            .field("operation", &self.operation)
            .field("cwd", &self.cwd)
            .field("argument_count", &self.args.len())
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

/// `ExecuteGitResult`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecuteGitResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

/// A test seam standing in for Effect's mockable `ChildProcessSpawner`: sees every command
/// once it holds its queue permit, before it spawns, and may delay it or replace its outcome.
#[async_trait]
pub trait GitInterceptor: Send + Sync {
    async fn intercept(&self, input: &ExecuteGitInput) -> Option<Result<ExecuteGitResult, GitCommandError>>;
}

fn git_processes() -> &'static Semaphore {
    static SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();
    SEMAPHORE.get_or_init(|| Semaphore::new(GIT_PROCESS_CONCURRENCY))
}

/// The available permits of the shared git queue (for tests).
pub fn available_git_permits() -> usize {
    git_processes().available_permits()
}

fn context_error(input: &ExecuteGitInput, detail: impl Into<String>) -> GitCommandError {
    GitCommandError::git(&input.operation, &input.cwd, input.args.len(), detail)
}

/// The git runner. Cheap to clone.
#[derive(Clone, Default)]
pub struct GitExecutor {
    interceptor: Option<Arc<dyn GitInterceptor>>,
    git_binary: Option<PathBuf>,
}

impl GitExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_interceptor(mut self, interceptor: Arc<dyn GitInterceptor>) -> Self {
        self.interceptor = Some(interceptor);
        self
    }

    /// Run a different `git` executable (tests).
    pub fn with_git_binary(mut self, binary: impl Into<PathBuf>) -> Self {
        self.git_binary = Some(binary.into());
        self
    }

    /// `execute`: queue (unless long-running), then run with the timeout.
    pub async fn execute(&self, input: ExecuteGitInput) -> Result<ExecuteGitResult, GitCommandError> {
        let _permit = if input.timeout.uses_queue() {
            Some(git_processes().acquire().await.expect("the git semaphore is never closed"))
        } else {
            None
        };
        tracing::trace!(operation = %input.operation, args = input.args.len(), "git");
        let run = self.run_with_interceptor(&input);
        match input.timeout.duration() {
            None => run.await,
            Some(limit) => match tokio::time::timeout(limit, run).await {
                Ok(result) => result,
                Err(_) => Err(context_error(&input, "Git command timed out.")),
            },
        }
    }

    async fn run_with_interceptor(&self, input: &ExecuteGitInput) -> Result<ExecuteGitResult, GitCommandError> {
        if let Some(interceptor) = &self.interceptor {
            if let Some(outcome) = interceptor.intercept(input).await {
                return outcome.and_then(|result| check_exit(input, result));
            }
        }
        let result = self.run(input).await?;
        check_exit(input, result)
    }

    async fn run(&self, input: &ExecuteGitInput) -> Result<ExecuteGitResult, GitCommandError> {
        let trace = Trace2Monitor::create(input)?;

        // Effect checks the working directory before spawning; a missing one is reported as a
        // spawn failure the status readers treat as "not a repository".
        match tokio::fs::metadata(&input.cwd).await {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                let not_a_directory = std::io::Error::from(std::io::ErrorKind::NotADirectory);
                let mut error = context_error(input, "Failed to spawn Git process.").with_cause(platform_error_defect("access", &input.cwd, &not_a_directory));
                error.missing_cwd = true;
                return Err(error);
            }
            Err(io) => {
                let mut error = context_error(input, "Failed to spawn Git process.").with_cause(platform_error_defect("access", &input.cwd, &io));
                error.missing_cwd = io.kind() == std::io::ErrorKind::NotFound;
                return Err(error);
            }
        }

        let binary = self.git_binary.clone().unwrap_or_else(|| PathBuf::from("git"));
        let mut command = Command::new(binary);
        command
            .args(&input.args)
            .current_dir(&input.cwd)
            .stdin(if input.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(overlay) = &input.env {
            for (key, value) in overlay {
                match value {
                    Some(value) => command.env(key, value),
                    None => command.env_remove(key),
                };
            }
        }
        if let Some(trace) = &trace {
            command.env("GIT_TRACE2_EVENT", &trace.path);
        }
        #[cfg(unix)]
        {
            // SAFETY: setsid is async-signal-safe; a new process group lets a timeout or a
            // cancellation terminate git together with the hooks and helpers it started.
            unsafe {
                command.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }
        let mut child = command
            .spawn()
            .map_err(|io| context_error(input, "Failed to spawn Git process.").with_cause(Defect::from(&io)))?;
        let mut guard = GroupGuard { pid: child.id(), done: false };

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdin = child.stdin.take();
        let progress = input.progress.clone().unwrap_or_default();
        let max = input.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES);

        let stdin_task = async {
            if let (Some(text), Some(mut pipe)) = (&input.stdin, stdin) {
                let written = async {
                    pipe.write_all(text.as_bytes()).await?;
                    pipe.shutdown().await
                }
                .await;
                drop(pipe);
                written.map_err(|io| context_error(input, "Failed to write Git process input.").with_cause(Defect::from(&io)))?;
            }
            Ok::<(), GitCommandError>(())
        };
        let stdout_task = collect_output(input, stdout, max, progress.on_stdout_line.clone());
        let stderr_task = collect_output(input, stderr, max, progress.on_stderr_line.clone());
        let trace_task = async {
            if let Some(trace) = &trace {
                trace.tail_until_cancelled().await;
            }
        };
        let work = async {
            let (stdout, stderr, ()) = tokio::try_join!(stdout_task, stderr_task, stdin_task)?;
            let status = child
                .wait()
                .await
                .map_err(|io| context_error(input, "Failed to read Git process exit code.").with_cause(Defect::from(&io)))?;
            Ok::<_, GitCommandError>((stdout, stderr, status))
        };
        let (outcome, ()) = tokio::join!(
            async {
                let outcome = work.await;
                if let Some(trace) = &trace {
                    trace.stop();
                }
                outcome
            },
            trace_task
        );
        let (stdout, stderr, status) = outcome?;
        guard.done = true;
        if let Some(trace) = &trace {
            trace.flush_final();
        }
        let exit_code = match status.code() {
            Some(code) => code,
            None => {
                return Err(context_error(input, "Failed to read Git process exit code.")
                    .with_cause(Defect::error("Error", "Process interrupted due to receipt of a signal")))
            }
        };
        Ok(ExecuteGitResult {
            exit_code,
            stdout: stdout.text,
            stderr: stderr.text,
            stdout_truncated: stdout.truncated,
            stderr_truncated: stderr.truncated,
        })
    }
}

fn check_exit(input: &ExecuteGitInput, result: ExecuteGitResult) -> Result<ExecuteGitResult, GitCommandError> {
    if !input.allow_non_zero_exit && result.exit_code != 0 {
        return Err(GitCommandError {
            exit_code: Some(result.exit_code),
            stdout_length: Some(js_length(&result.stdout)),
            stderr_length: Some(js_length(&result.stderr)),
            ..context_error(input, "Git command exited with a non-zero status.")
        });
    }
    Ok(result)
}

/// Terminates the process group when a run is abandoned (timeout, cancellation, error).
struct GroupGuard {
    pid: Option<u32>,
    done: bool,
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            // SAFETY: plain syscall; a negative pid addresses the process group.
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
            }
        }
    }
}

struct Collected {
    text: String,
    truncated: bool,
}

/// Splits complete lines off `buffer` (`\r\n`, `\r` or `\n`) and hands the non-empty ones to
/// the callback. With `flush`, the remainder is a last line.
fn emit_complete_lines(buffer: &mut Vec<u8>, on_line: Option<&LineCallback>, flush: bool) {
    let mut start = 0;
    let mut index = 0;
    while index < buffer.len() {
        let byte = buffer[index];
        if byte == b'\n' || byte == b'\r' {
            let separator = if byte == b'\r' && buffer.get(index + 1) == Some(&b'\n') { 2 } else { 1 };
            if index > start {
                if let Some(on_line) = on_line {
                    on_line(&String::from_utf8_lossy(&buffer[start..index]));
                }
            }
            index += separator;
            start = index;
        } else {
            index += 1;
        }
    }
    buffer.drain(..start);
    if flush {
        if !buffer.is_empty() {
            if let Some(on_line) = on_line {
                on_line(&String::from_utf8_lossy(buffer));
            }
        }
        buffer.clear();
    }
}

async fn collect_output<R: AsyncRead + Unpin>(
    input: &ExecuteGitInput,
    stream: Option<R>,
    max_output_bytes: usize,
    on_line: Option<LineCallback>,
) -> Result<Collected, GitCommandError> {
    let append_marker = input.append_truncation_marker;
    let keep_lines = input.keep_line_callbacks_after_truncation && on_line.is_some();
    let mut text: Vec<u8> = Vec::new();
    let mut lines: Vec<u8> = Vec::new();
    let mut bytes = 0usize;
    let mut truncated = false;
    let mut bom_handled = false;
    let Some(mut stream) = stream else {
        return Ok(Collected {
            text: String::new(),
            truncated: false,
        });
    };
    let mut chunk = vec![0u8; READ_CHUNK_BYTES];
    loop {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|io| context_error(input, "Failed to read Git process output.").with_cause(Defect::from(&io)))?;
        if read == 0 {
            break;
        }
        let chunk = &chunk[..read];
        if append_marker && truncated {
            if keep_lines {
                lines.extend_from_slice(chunk);
                emit_complete_lines(&mut lines, on_line.as_ref(), false);
                if lines.len() > MAX_PENDING_LINE_BYTES {
                    lines.clear();
                }
            }
            continue;
        }
        let next_bytes = bytes + chunk.len();
        if !append_marker && next_bytes > max_output_bytes {
            return Err(GitCommandError {
                output_length: Some(next_bytes),
                ..context_error(input, format!("Git output exceeded {max_output_bytes} bytes and was truncated."))
            });
        }
        let to_decode = if append_marker && next_bytes > max_output_bytes {
            &chunk[..max_output_bytes.saturating_sub(bytes)]
        } else {
            chunk
        };
        bytes += to_decode.len();
        truncated = append_marker && next_bytes > max_output_bytes;
        text.extend_from_slice(to_decode);
        lines.extend_from_slice(if keep_lines { chunk } else { to_decode });
        if !bom_handled && lines.len() >= UTF8_BOM.len() {
            // `TextDecoder` drops a leading byte order mark.
            if lines.starts_with(UTF8_BOM) {
                lines.drain(..UTF8_BOM.len());
            }
            bom_handled = true;
        }
        emit_complete_lines(&mut lines, on_line.as_ref(), false);
    }
    emit_complete_lines(&mut lines, on_line.as_ref(), true);
    let text = text.strip_prefix(UTF8_BOM).unwrap_or(&text);
    Ok(Collected {
        text: String::from_utf8_lossy(text).into_owned(),
        truncated,
    })
}

// ---------------------------------------------------------------------------------------------
// trace2 hook monitor
// ---------------------------------------------------------------------------------------------

struct HookStart {
    hook_name: String,
    started_at_ms: i64,
}

struct TraceState {
    offset: u64,
    remainder: Vec<u8>,
    starts: HashMap<String, HookStart>,
}

struct Trace2Monitor {
    path: PathBuf,
    state: Mutex<TraceState>,
    stopped: tokio::sync::Notify,
    stop_flag: std::sync::atomic::AtomicBool,
    on_started: Option<HookStartedCallback>,
    on_finished: Option<HookFinishedCallback>,
    operation: String,
}

impl Trace2Monitor {
    fn create(input: &ExecuteGitInput) -> Result<Option<Self>, GitCommandError> {
        let Some(progress) = &input.progress else {
            return Ok(None);
        };
        if progress.on_hook_started.is_none() && progress.on_hook_finished.is_none() {
            return Ok(None);
        }
        let path = std::env::temp_dir().join(format!("t3code-git-trace2-{}-{}.json", std::process::id(), uuid::Uuid::new_v4().simple()));
        std::fs::File::create(&path).map_err(|io| context_error(input, "Failed to create Git trace monitor.").with_cause(Defect::from(&io)))?;
        Ok(Some(Self {
            path,
            state: Mutex::new(TraceState {
                offset: 0,
                remainder: Vec::new(),
                starts: HashMap::new(),
            }),
            stopped: tokio::sync::Notify::new(),
            stop_flag: std::sync::atomic::AtomicBool::new(false),
            on_started: progress.on_hook_started.clone(),
            on_finished: progress.on_hook_finished.clone(),
            operation: input.operation.clone(),
        }))
    }

    fn stop(&self) {
        self.stop_flag.store(true, std::sync::atomic::Ordering::SeqCst);
        self.stopped.notify_waiters();
    }

    async fn tail_until_cancelled(&self) {
        loop {
            if self.stop_flag.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            self.read_delta();
            tokio::select! {
                _ = tokio::time::sleep(TRACE_POLL_INTERVAL) => {}
                _ = self.stopped.notified() => return,
            }
        }
    }

    fn read_delta(&self) {
        use std::io::{Read, Seek, SeekFrom};
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return;
        };
        if file.seek(SeekFrom::Start(state.offset)).is_err() {
            return;
        }
        let mut appended = Vec::new();
        if file.read_to_end(&mut appended).is_err() || appended.is_empty() {
            return;
        }
        state.offset += appended.len() as u64;
        state.remainder.extend_from_slice(&appended);
        let mut lines = Vec::new();
        while let Some(index) = state.remainder.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = state.remainder.drain(..=index).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            lines.push(String::from_utf8_lossy(&line).into_owned());
        }
        for line in lines {
            self.handle_line(&mut state, &line);
        }
    }

    fn flush_final(&self) {
        self.read_delta();
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let remainder = std::mem::take(&mut state.remainder);
        let line = String::from_utf8_lossy(&remainder).trim().to_owned();
        if !line.is_empty() {
            self.handle_line(&mut state, &line);
        }
    }

    fn handle_line(&self, state: &mut TraceState, line: &str) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        let record: serde_json::Map<String, serde_json::Value> = match serde_json::from_str(trimmed) {
            Ok(record) => record,
            Err(error) => {
                tracing::debug!(
                    operation = %self.operation,
                    %error,
                    "GitVcsDriver.trace2: failed to parse trace line"
                );
                return;
            }
        };
        if record.get("child_class").and_then(|v| v.as_str()) != Some("hook") {
            return;
        }
        let child_key = match record.get("child_id") {
            Some(serde_json::Value::Number(n)) => Some(n.to_string()),
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            _ => record
                .get("hook_name")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        };
        let Some(child_key) = child_key else {
            return;
        };
        let name_from_event = record.get("hook_name").and_then(|v| v.as_str()).map(str::trim).unwrap_or_default().to_owned();
        let hook_name = if !name_from_event.is_empty() {
            name_from_event
        } else {
            state.starts.get(&child_key).map(|start| start.hook_name.clone()).unwrap_or_default()
        };
        if hook_name.is_empty() {
            return;
        }
        match record.get("event").and_then(|v| v.as_str()) {
            Some("child_start") => {
                state.starts.insert(
                    child_key,
                    HookStart {
                        hook_name: hook_name.clone(),
                        started_at_ms: zc_core::time::now_millis(),
                    },
                );
                if let Some(on_started) = &self.on_started {
                    on_started(&hook_name);
                }
            }
            Some("child_exit") => {
                let started = state.starts.remove(&child_key);
                // TS reads `exitCode`, a field git's trace2 `child_exit` event does not have
                // (it writes `code`), so this is null in practice; kept for parity.
                let exit_code = record.get("exitCode").and_then(|v| match v {
                    serde_json::Value::Number(n) => n.as_i64(),
                    _ => None,
                });
                let duration_ms = started.as_ref().map(|start| (zc_core::time::now_millis() - start.started_at_ms).max(0) as u64);
                if let Some(on_finished) = &self.on_finished {
                    on_finished(HookFinished {
                        hook_name: started.map(|s| s.hook_name).unwrap_or(hook_name),
                        exit_code,
                        duration_ms,
                    });
                }
            }
            _ => {}
        }
    }
}

impl Drop for Trace2Monitor {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// `path.resolve(cwd, value)` for git output that may be relative.
pub fn resolve_against(cwd: &str, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        zc_core::paths::normalize_lexically(path)
    } else {
        zc_core::paths::normalize_lexically(&zc_core::paths::resolve_path(Path::new(cwd)).join(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_lines_on_every_separator() {
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let callback: LineCallback = Arc::new(move |line| sink.lock().unwrap().push(line.into()));
        let mut buffer = b"a\r\nb\rc\n\nd".to_vec();
        emit_complete_lines(&mut buffer, Some(&callback), false);
        assert_eq!(buffer, b"d");
        emit_complete_lines(&mut buffer, Some(&callback), true);
        assert_eq!(*seen.lock().unwrap(), vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn long_timeouts_skip_the_queue() {
        assert!(GitTimeout::Default.uses_queue());
        assert!(GitTimeout::Millis(30_000).uses_queue());
        assert!(!GitTimeout::Millis(300_000).uses_queue());
        assert!(!GitTimeout::Unbounded.uses_queue());
    }
}
