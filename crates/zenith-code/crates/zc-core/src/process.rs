//! The process runner (`apps/server/src/processRunner.ts` on Effect's `NodeChildProcessSpawner`).
//!
//! Every subprocess of the server goes through this: run a command to completion, collect stdout
//! and stderr with a byte cap, optionally feed stdin, enforce a timeout. Semantics kept from TS:
//!
//! - **Default timeout 60 s**, measured over spawn + output collection + exit. On timeout the
//!   result is a [`ProcessRunError::Timeout`], or an all-empty `timed_out` output when
//!   [`TimeoutBehavior::TimedOutResult`] is asked for (partial output is *not* kept).
//! - **Output cap 8 MiB per stream** by default. [`OutputMode::Error`] fails as soon as a chunk
//!   would cross the cap (the child is then terminated); [`OutputMode::Truncate`] keeps the first
//!   `max_output_bytes` bytes, keeps draining so the child can exit normally, sets the
//!   `*_truncated` flag and appends `truncated_marker` (if non-empty).
//! - **`*_invalid_utf8`** says whether the collected (possibly truncated) bytes were valid UTF-8;
//!   the text is decoded lossily (U+FFFD), like `Buffer.toString("utf8")`.
//! - **stdin**: when given, written in full and closed concurrently with output collection. When
//!   not given the pipe stays open, unwritten, until the process exits (that is what Effect's
//!   default `"pipe"` stdin does; a child that reads stdin waits for the timeout).
//! - **env** extends the parent environment (`extendEnv`); `None` values remove a variable.
//! - **Process groups**: the child runs in its own session (`detached: true` on POSIX means
//!   `setsid`), so it has no controlling terminal and can be signalled as a group. When the run
//!   is abandoned (timeout, output limit, read error) the whole group gets `SIGTERM`, the runner
//!   waits up to 1 s for it to go away, then (Rust addition) sends `SIGKILL` so a child that
//!   ignores `SIGTERM` cannot hang the caller. After a normal exit with a non-zero code the group
//!   gets a best-effort `SIGTERM`, which reaps orphaned grandchildren.
//! - A child killed by a signal has no exit code: that is a [`ProcessRunError::Read`] on the
//!   `exitCode` stream, like Effect's `exitCode` failure.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

use crate::defect::Defect;

/// Default timeout of [`run_process`] (`DEFAULT_TIMEOUT = "60 seconds"`).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// Default per-stream output cap (`8 * 1024 * 1024`).
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
/// How long a terminated process group gets before the Rust runner escalates to `SIGKILL`
/// (Effect's `processGroupGraceMillis`).
pub const PROCESS_GROUP_GRACE: Duration = Duration::from_millis(1_000);
const PROCESS_GROUP_POLL_INTERVAL: Duration = Duration::from_millis(10);
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// Environment changes applied on top of the server's own environment. `None` unsets a variable.
pub type EnvOverlay = BTreeMap<String, Option<String>>;

/// Receives every stdout chunk, including bytes beyond the buffered output limit.
pub type ChunkCallback = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// What happens when output exceeds `max_output_bytes`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OutputMode {
    /// Fail with [`ProcessRunError::OutputLimit`] (the TS default).
    #[default]
    Error,
    /// Keep the first `max_output_bytes` bytes and flag the stream as truncated.
    Truncate,
}

/// What happens on timeout.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TimeoutBehavior {
    /// Fail with [`ProcessRunError::Timeout`] (the TS default).
    #[default]
    Error,
    /// Succeed with an empty output whose `timed_out` is true and `code` is `None`.
    TimedOutResult,
}

/// One process invocation (`ProcessRunInput`).
#[derive(Clone, Default)]
pub struct ProcessRunInput {
    pub command: String,
    pub args: Vec<String>,
    /// The logical working directory (reported in errors). Also the spawn directory unless
    /// `spawn_cwd` is set.
    pub cwd: Option<PathBuf>,
    /// Where the process actually runs, when it differs from `cwd`.
    pub spawn_cwd: Option<PathBuf>,
    /// Defaults to [`DEFAULT_TIMEOUT`].
    pub timeout: Option<Duration>,
    pub env: Option<EnvOverlay>,
    pub stdin: Option<String>,
    pub on_stdout_chunk: Option<ChunkCallback>,
    /// Defaults to [`DEFAULT_MAX_OUTPUT_BYTES`].
    pub max_output_bytes: Option<usize>,
    pub output_mode: OutputMode,
    /// Appended to truncated text in [`OutputMode::Truncate`]. Defaults to empty.
    pub truncated_marker: Option<String>,
    pub timeout_behavior: TimeoutBehavior,
}

impl ProcessRunInput {
    pub fn new<I, S>(command: impl Into<String>, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            command: command.into(),
            args: args.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }

    fn invocation(&self) -> ProcessInvocation {
        ProcessInvocation {
            command: self.command.clone(),
            argument_count: self.args.len(),
            cwd: self.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
            spawn_cwd: self.spawn_cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
        }
    }
}

impl fmt::Debug for ProcessRunInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Arguments and stdin can hold secrets: show only their sizes.
        f.debug_struct("ProcessRunInput")
            .field("command", &self.command)
            .field("argument_count", &self.args.len())
            .field("cwd", &self.cwd)
            .field("spawn_cwd", &self.spawn_cwd)
            .field("timeout", &self.timeout)
            .field("stdin_bytes", &self.stdin.as_ref().map(String::len))
            .field("max_output_bytes", &self.max_output_bytes)
            .field("output_mode", &self.output_mode)
            .field("timeout_behavior", &self.timeout_behavior)
            .finish_non_exhaustive()
    }
}

/// The collected result (`ProcessRunOutput`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessRunOutput {
    pub stdout: String,
    pub stderr: String,
    /// `None` only for a synthetic timed-out result.
    pub code: Option<i32>,
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub stdout_invalid_utf8: bool,
    pub stderr_invalid_utf8: bool,
}

/// The fields every process error carries (`ProcessInvocationFields`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInvocation {
    pub command: String,
    pub argument_count: usize,
    pub cwd: Option<String>,
    pub spawn_cwd: Option<String>,
}

impl ProcessInvocation {
    /// `formatProcessInvocation`: `'cmd'` or `'cmd' in '<dir>'`.
    pub fn describe(&self) -> String {
        match self.spawn_cwd.as_ref().or(self.cwd.as_ref()) {
            Some(dir) => format!("'{}' in '{}'", self.command, dir),
            None => format!("'{}'", self.command),
        }
    }
}

/// Which stream a read error or limit error is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStream {
    Stdout,
    Stderr,
    ExitCode,
}

impl ProcessStream {
    /// The wire literal (`"stdout" | "stderr" | "exitCode"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::ExitCode => "exitCode",
        }
    }
}

impl fmt::Display for ProcessStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `ProcessRunError`: the five tagged errors of `processRunner.ts`, same messages.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ProcessRunError {
    /// `ProcessSpawnError`.
    #[error("Failed to spawn process {}", .invocation.describe())]
    Spawn {
        invocation: ProcessInvocation,
        resolved_command: Option<String>,
        resolved_argument_count: Option<usize>,
        shell: Option<bool>,
        cause: Defect,
    },
    /// `ProcessStdinError`.
    #[error("Failed to write stdin for process {}", .invocation.describe())]
    Stdin {
        invocation: ProcessInvocation,
        stdin_bytes: usize,
        cause: Defect,
    },
    /// `ProcessOutputLimitError`.
    #[error(
        "Process {} {} produced {} bytes, exceeding the {} byte limit",
        .invocation.describe(), .stream, .observed_bytes, .max_bytes
    )]
    OutputLimit {
        invocation: ProcessInvocation,
        stream: ProcessStream,
        max_bytes: usize,
        observed_bytes: usize,
    },
    /// `ProcessReadError`.
    #[error("Failed to read {} for process {}", .stream, .invocation.describe())]
    Read {
        invocation: ProcessInvocation,
        stream: ProcessStream,
        cause: Defect,
    },
    /// `ProcessTimeoutError`.
    #[error("Process {} timed out after {}ms", .invocation.describe(), .timeout_ms)]
    Timeout { invocation: ProcessInvocation, timeout_ms: u64 },
}

impl ProcessRunError {
    /// The TS `_tag` of this error.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Spawn { .. } => "ProcessSpawnError",
            Self::Stdin { .. } => "ProcessStdinError",
            Self::OutputLimit { .. } => "ProcessOutputLimitError",
            Self::Read { .. } => "ProcessReadError",
            Self::Timeout { .. } => "ProcessTimeoutError",
        }
    }

    pub fn invocation(&self) -> &ProcessInvocation {
        match self {
            Self::Spawn { invocation, .. }
            | Self::Stdin { invocation, .. }
            | Self::OutputLimit { invocation, .. }
            | Self::Read { invocation, .. }
            | Self::Timeout { invocation, .. } => invocation,
        }
    }
}

/// The `ProcessRunner` service: injectable so other crates can fake subprocesses in tests.
#[async_trait]
pub trait ProcessRunner: Send + Sync {
    async fn run(&self, input: ProcessRunInput) -> Result<ProcessRunOutput, ProcessRunError>;
}

/// The real runner: spawns OS processes with [`run_process`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemProcessRunner;

#[async_trait]
impl ProcessRunner for SystemProcessRunner {
    async fn run(&self, input: ProcessRunInput) -> Result<ProcessRunOutput, ProcessRunError> {
        run_process(input).await
    }
}

/// `commandName`: the executable name without its directory.
pub fn command_name(command: &str) -> &str {
    command.rsplit(['/', '\\']).next().unwrap_or(command)
}

/// Run one process to completion. See the module docs for the exact semantics.
pub async fn run_process(input: ProcessRunInput) -> Result<ProcessRunOutput, ProcessRunError> {
    let timeout = input.timeout.unwrap_or(DEFAULT_TIMEOUT);
    let deadline = tokio::time::Instant::now() + timeout;
    let mut slot: Option<RunningChild> = None;
    let outcome = tokio::time::timeout_at(deadline, run_core(&input, &mut slot)).await;
    match outcome {
        Ok(Ok(output)) => {
            if let Some(running) = slot.take() {
                running.after_exit(output.code);
            }
            Ok(output)
        }
        Ok(Err(error)) => {
            if let Some(running) = slot.take() {
                running.abandon().await;
            }
            Err(error)
        }
        Err(_elapsed) => {
            if let Some(running) = slot.take() {
                running.abandon().await;
            }
            match input.timeout_behavior {
                TimeoutBehavior::TimedOutResult => Ok(ProcessRunOutput {
                    timed_out: true,
                    ..ProcessRunOutput::default()
                }),
                TimeoutBehavior::Error => Err(ProcessRunError::Timeout {
                    invocation: input.invocation(),
                    timeout_ms: timeout.as_millis() as u64,
                }),
            }
        }
    }
}

struct RunningChild {
    child: Child,
    pid: Option<u32>,
}

impl RunningChild {
    /// After a normal exit: a non-zero code gets a best-effort `SIGTERM` to the group, which
    /// reaps grandchildren the command left behind (Effect's `killProcessGroupOnExit`).
    fn after_exit(self, code: Option<i32>) {
        if code.is_some_and(|code| code != 0) {
            if let Some(pid) = self.pid {
                signal_group(pid, libc::SIGTERM);
            }
        }
    }

    /// Effect's scope finalizer for an unfinished run: terminate the process group.
    async fn abandon(mut self) {
        match self.child.try_wait() {
            Ok(Some(status)) => {
                let code = status.code();
                self.after_exit(code);
            }
            _ => self.terminate().await,
        }
    }

    async fn terminate(&mut self) {
        let Some(pid) = self.pid else {
            let _ = self.child.start_kill();
            let _ = self.child.wait().await;
            return;
        };
        if !signal_group(pid, libc::SIGTERM) {
            signal_pid(pid, libc::SIGTERM);
        }
        if !self.await_group_exit(pid, PROCESS_GROUP_GRACE).await {
            // Rust addition: Effect would wait for the leader forever here.
            if !signal_group(pid, libc::SIGKILL) {
                let _ = self.child.start_kill();
            }
            self.await_group_exit(pid, PROCESS_GROUP_GRACE).await;
        }
        let _ = self.child.wait().await;
    }

    /// Poll until the leader has exited and its group is gone, or the time is up.
    async fn await_group_exit(&mut self, pid: u32, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let leader_exited = matches!(self.child.try_wait(), Ok(Some(_)));
            if leader_exited && !group_exists(pid) {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(PROCESS_GROUP_POLL_INTERVAL).await;
        }
    }
}

#[cfg(unix)]
fn signal_group(pid: u32, signal: libc::c_int) -> bool {
    // SAFETY: plain syscall; a negative pid addresses the process group.
    unsafe { libc::kill(-(pid as libc::pid_t), signal) == 0 }
}

#[cfg(unix)]
fn signal_pid(pid: u32, signal: libc::c_int) -> bool {
    // SAFETY: plain syscall.
    unsafe { libc::kill(pid as libc::pid_t, signal) == 0 }
}

#[cfg(unix)]
fn group_exists(pid: u32) -> bool {
    // SAFETY: signal 0 only checks for existence.
    unsafe { libc::kill(-(pid as libc::pid_t), 0) == 0 }
}

#[cfg(not(unix))]
fn signal_group(_pid: u32, _signal: i32) -> bool {
    false
}

#[cfg(not(unix))]
fn signal_pid(_pid: u32, _signal: i32) -> bool {
    false
}

#[cfg(not(unix))]
fn group_exists(_pid: u32) -> bool {
    false
}

#[cfg(not(unix))]
#[allow(non_camel_case_types, dead_code)]
mod libc {
    pub type c_int = i32;
    pub const SIGTERM: c_int = 15;
    pub const SIGKILL: c_int = 9;
}

async fn run_core(input: &ProcessRunInput, slot: &mut Option<RunningChild>) -> Result<ProcessRunOutput, ProcessRunError> {
    let invocation = input.invocation();
    let max_output_bytes = input.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES);
    let marker = input.truncated_marker.clone().unwrap_or_default();

    let spawn_error = |cause: Defect| ProcessRunError::Spawn {
        invocation: invocation.clone(),
        resolved_command: Some(input.command.clone()),
        resolved_argument_count: Some(input.args.len()),
        shell: Some(false),
        cause,
    };

    let mut command = Command::new(&input.command);
    command.args(&input.args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(dir) = input.spawn_cwd.as_ref().or(input.cwd.as_ref()) {
        // Effect validates the directory (`fs.access`) and resolves it before spawning.
        if let Err(error) = tokio::fs::metadata(dir).await {
            return Err(spawn_error(Defect::from(&error)));
        }
        command.current_dir(crate::paths::resolve_path(Path::new(dir)));
    }
    if let Some(env) = &input.env {
        for (key, value) in env {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
        }
    }
    #[cfg(unix)]
    {
        // SAFETY: setsid is async-signal-safe; this mirrors Node's `detached: true` on POSIX.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let mut child = command.spawn().map_err(|error| spawn_error(Defect::from(&error)))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdin = child.stdin.take();
    let pid = child.id();
    let running = slot.insert(RunningChild { child, pid });

    let stdin_task = async {
        match (&input.stdin, stdin) {
            (Some(text), Some(mut pipe)) => {
                let result = async {
                    pipe.write_all(text.as_bytes()).await?;
                    pipe.shutdown().await
                }
                .await;
                drop(pipe);
                result.map(|()| None).map_err(|error| ProcessRunError::Stdin {
                    invocation: invocation.clone(),
                    stdin_bytes: text.len(),
                    cause: Defect::from(&error),
                })
            }
            // No stdin: keep the pipe open, unwritten, until the process is done.
            (None, pipe) => Ok(pipe),
            (Some(_), None) => Ok(None),
        }
    };
    let stdout_task = collect_stream(
        stdout,
        ProcessStream::Stdout,
        max_output_bytes,
        input.output_mode,
        &marker,
        input.on_stdout_chunk.clone(),
        &invocation,
    );
    let stderr_task = collect_stream(stderr, ProcessStream::Stderr, max_output_bytes, input.output_mode, &marker, None, &invocation);
    let (stdout, stderr, held_stdin) = tokio::try_join!(stdout_task, stderr_task, stdin_task)?;

    let status = running.child.wait().await.map_err(|error| ProcessRunError::Read {
        invocation: invocation.clone(),
        stream: ProcessStream::ExitCode,
        cause: Defect::from(&error),
    })?;
    drop(held_stdin);
    let code = match status.code() {
        Some(code) => code,
        None => {
            return Err(ProcessRunError::Read {
                invocation: invocation.clone(),
                stream: ProcessStream::ExitCode,
                cause: Defect::error(
                    "Error",
                    format!("Process interrupted due to receipt of signal: '{}'", exit_signal_name(&status)),
                ),
            })
        }
    };

    Ok(ProcessRunOutput {
        stdout: stdout.text,
        stderr: stderr.text,
        code: Some(code),
        timed_out: false,
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
        stdout_invalid_utf8: stdout.invalid_utf8,
        stderr_invalid_utf8: stderr.invalid_utf8,
    })
}

#[cfg(unix)]
fn exit_signal_name(status: &std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(signal_name).unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(not(unix))]
fn exit_signal_name(_status: &std::process::ExitStatus) -> String {
    "unknown".to_owned()
}

/// Node's name for a POSIX signal number.
pub fn signal_name(signal: i32) -> String {
    let name = match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        5 => "SIGTRAP",
        6 => "SIGABRT",
        8 => "SIGFPE",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        _ => return format!("SIG{signal}"),
    };
    name.to_owned()
}

/// Text collected from one output stream (`CollectedUint8StreamText`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CollectedText {
    pub text: String,
    pub truncated: bool,
    pub bytes: usize,
    pub invalid_utf8: bool,
}

/// Decode bytes like `decodeUtf8`: lossy text plus a validity flag.
pub fn decode_utf8(bytes: &[u8]) -> (String, bool) {
    match std::str::from_utf8(bytes) {
        Ok(text) => (text.to_owned(), false),
        Err(_) => (String::from_utf8_lossy(bytes).into_owned(), true),
    }
}

/// Byte accumulator with the exact cap rules of `collectUint8StreamText` (truncate) and the
/// `runFoldEffect` limit check (error).
#[derive(Debug, Default)]
pub struct OutputCollector {
    chunks: Vec<u8>,
    truncated: bool,
    max_bytes: usize,
    mode: OutputMode,
}

impl OutputCollector {
    pub fn new(max_bytes: usize, mode: OutputMode) -> Self {
        Self {
            chunks: Vec::new(),
            truncated: false,
            max_bytes,
            mode,
        }
    }

    /// Feed one chunk. In error mode returns `Err(observed_bytes)` when the cap is crossed.
    pub fn push(&mut self, chunk: &[u8]) -> Result<(), usize> {
        let held = self.chunks.len();
        match self.mode {
            OutputMode::Error => {
                if chunk.len() > self.max_bytes.saturating_sub(held) {
                    return Err(held + chunk.len());
                }
                self.chunks.extend_from_slice(chunk);
            }
            OutputMode::Truncate => {
                if self.truncated {
                    return Ok(());
                }
                let remaining = self.max_bytes.saturating_sub(held);
                if remaining == 0 {
                    // Mirrors `remainingBytes <= 0` (an empty chunk at the cap also flips it).
                    self.truncated = true;
                    return Ok(());
                }
                let take = chunk.len().min(remaining);
                self.chunks.extend_from_slice(&chunk[..take]);
                self.truncated = chunk.len() > remaining;
            }
        }
        Ok(())
    }

    pub fn finish(self, marker: &str) -> CollectedText {
        let (text, invalid_utf8) = decode_utf8(&self.chunks);
        let text = if self.truncated && !marker.is_empty() {
            format!("{text}{marker}")
        } else {
            text
        };
        CollectedText {
            text,
            truncated: self.truncated,
            bytes: self.chunks.len(),
            invalid_utf8,
        }
    }
}

async fn collect_stream<R: AsyncRead + Unpin>(
    reader: Option<R>,
    stream: ProcessStream,
    max_bytes: usize,
    mode: OutputMode,
    marker: &str,
    on_chunk: Option<ChunkCallback>,
    invocation: &ProcessInvocation,
) -> Result<CollectedText, ProcessRunError> {
    let mut collector = OutputCollector::new(max_bytes, mode);
    let Some(mut reader) = reader else {
        return Ok(collector.finish(marker));
    };
    let mut buffer = vec![0u8; READ_CHUNK_BYTES];
    loop {
        let read = reader.read(&mut buffer).await.map_err(|error| ProcessRunError::Read {
            invocation: invocation.clone(),
            stream,
            cause: Defect::from(&error),
        })?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        if let Some(callback) = &on_chunk {
            callback(chunk);
        }
        collector.push(chunk).map_err(|observed_bytes| ProcessRunError::OutputLimit {
            invocation: invocation.clone(),
            stream,
            max_bytes,
            observed_bytes,
        })?;
    }
    Ok(collector.finish(marker))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn sh(script: &str) -> ProcessRunInput {
        ProcessRunInput::new("/bin/sh", ["-c", script])
    }

    #[tokio::test]
    async fn collects_stdout_stderr_and_exit_code() {
        let output = run_process(sh("printf out; printf err >&2; exit 3")).await.unwrap();
        assert_eq!(output.stdout, "out");
        assert_eq!(output.stderr, "err");
        assert_eq!(output.code, Some(3));
        assert!(!output.timed_out && !output.stdout_truncated && !output.stdout_invalid_utf8);
    }

    #[tokio::test]
    async fn writes_stdin_and_closes_it() {
        let mut input = ProcessRunInput::new("/bin/cat", Vec::<String>::new());
        input.stdin = Some("hello\nworld".into());
        let output = run_process(input).await.unwrap();
        assert_eq!(output.stdout, "hello\nworld");
        assert_eq!(output.code, Some(0));
    }

    #[tokio::test]
    async fn times_out_with_an_error_and_kills_the_group() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        let mut input = sh(&format!("(sleep 2; touch '{}') & sleep 30", marker.display()));
        input.timeout = Some(Duration::from_millis(200));
        let started = std::time::Instant::now();
        let error = run_process(input).await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        match &error {
            ProcessRunError::Timeout { timeout_ms, .. } => assert_eq!(*timeout_ms, 200),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(error.to_string(), "Process '/bin/sh' timed out after 200ms");
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        assert!(!marker.exists(), "the background grandchild should have been killed with the group");
    }

    #[tokio::test]
    async fn timed_out_result_mode_returns_an_empty_output() {
        let mut input = sh("printf partial; sleep 30");
        input.timeout = Some(Duration::from_millis(150));
        input.timeout_behavior = TimeoutBehavior::TimedOutResult;
        let output = run_process(input).await.unwrap();
        assert!(output.timed_out);
        assert_eq!(output.code, None);
        assert_eq!(output.stdout, "");
    }

    #[tokio::test]
    async fn sigterm_ignoring_children_are_escalated_to_sigkill() {
        let mut input = sh("trap '' TERM; sleep 30");
        input.timeout = Some(Duration::from_millis(100));
        let started = std::time::Instant::now();
        assert!(matches!(run_process(input).await, Err(ProcessRunError::Timeout { .. })));
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[tokio::test]
    async fn output_limit_fails_fast_in_error_mode() {
        let mut input = sh("head -c 5000 /dev/zero; sleep 30");
        input.max_output_bytes = Some(1000);
        input.timeout = Some(Duration::from_secs(20));
        let started = std::time::Instant::now();
        let error = run_process(input).await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        match error {
            ProcessRunError::OutputLimit {
                stream,
                max_bytes,
                observed_bytes,
                ..
            } => {
                assert_eq!(stream, ProcessStream::Stdout);
                assert_eq!(max_bytes, 1000);
                assert!(observed_bytes > 1000);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn output_exactly_at_the_limit_is_accepted() {
        let mut input = sh("head -c 1000 /dev/zero");
        input.max_output_bytes = Some(1000);
        let output = run_process(input).await.unwrap();
        assert_eq!(output.stdout.len(), 1000);
        assert!(!output.stdout_truncated);
    }

    #[tokio::test]
    async fn truncate_mode_keeps_draining_and_appends_the_marker() {
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = seen.clone();
        let mut input = sh("head -c 200000 /dev/zero | tr '\\0' a; printf tail >&2");
        input.max_output_bytes = Some(10);
        input.output_mode = OutputMode::Truncate;
        input.truncated_marker = Some("\n\n[truncated]".into());
        input.on_stdout_chunk = Some(Arc::new(move |chunk: &[u8]| {
            counter.fetch_add(chunk.len(), Ordering::SeqCst);
        }));
        let output = run_process(input).await.unwrap();
        assert_eq!(output.stdout, "aaaaaaaaaa\n\n[truncated]");
        assert!(output.stdout_truncated);
        assert_eq!(output.stderr, "tail");
        assert!(!output.stderr_truncated);
        assert_eq!(output.code, Some(0));
        assert_eq!(seen.load(Ordering::SeqCst), 200_000, "every chunk reaches the callback");
    }

    #[tokio::test]
    async fn flags_invalid_utf8() {
        let output = run_process(sh("printf 'ok\\377'")).await.unwrap();
        assert!(output.stdout_invalid_utf8);
        assert_eq!(output.stdout, "ok\u{FFFD}");
        assert!(!output.stderr_invalid_utf8);
    }

    #[tokio::test]
    async fn truncation_inside_a_multibyte_char_flags_invalid_utf8() {
        let mut input = sh("printf 'aé'");
        input.max_output_bytes = Some(2);
        input.output_mode = OutputMode::Truncate;
        let output = run_process(input).await.unwrap();
        assert!(output.stdout_truncated);
        assert!(output.stdout_invalid_utf8);
    }

    #[tokio::test]
    async fn env_overlay_extends_and_removes() {
        std::env::set_var("ZC_CORE_TEST_INHERITED", "inherited");
        let mut input = sh("printf '%s|%s|%s' \"$ZC_CORE_TEST_INHERITED\" \"$ZC_ADDED\" \"${HOME:-unset}\"");
        input.env = Some(EnvOverlay::from([("ZC_ADDED".to_owned(), Some("added".to_owned())), ("HOME".to_owned(), None)]));
        let output = run_process(input).await.unwrap();
        assert_eq!(output.stdout, "inherited|added|unset");
    }

    #[tokio::test]
    async fn runs_in_the_spawn_cwd_and_reports_the_logical_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let mut input = ProcessRunInput::new("/bin/pwd", Vec::<String>::new());
        input.cwd = Some(PathBuf::from("/logical"));
        input.spawn_cwd = Some(dir.path().to_path_buf());
        let output = run_process(input).await.unwrap();
        let reported = std::fs::canonicalize(output.stdout.trim()).unwrap();
        assert_eq!(reported, std::fs::canonicalize(dir.path()).unwrap());
    }

    #[tokio::test]
    async fn missing_cwd_and_missing_binary_are_spawn_errors() {
        let mut input = ProcessRunInput::new("/bin/pwd", Vec::<String>::new());
        input.cwd = Some(PathBuf::from("/definitely/not/here"));
        let error = run_process(input).await.unwrap_err();
        assert_eq!(error.tag(), "ProcessSpawnError");
        assert_eq!(error.to_string(), "Failed to spawn process '/bin/pwd' in '/definitely/not/here'");
        let error = run_process(ProcessRunInput::new("zc-core-no-such-binary", ["x"])).await.unwrap_err();
        assert_eq!(error.tag(), "ProcessSpawnError");
        assert_eq!(error.invocation().argument_count, 1);
    }

    #[tokio::test]
    async fn signal_death_is_an_exit_code_read_error() {
        let error = run_process(sh("kill -KILL $$")).await.unwrap_err();
        match error {
            ProcessRunError::Read { stream, cause, .. } => {
                assert_eq!(stream, ProcessStream::ExitCode);
                assert_eq!(cause.message(), "Process interrupted due to receipt of signal: 'SIGKILL'");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn collector_matches_ts_cap_rules() {
        let mut truncating = OutputCollector::new(4, OutputMode::Truncate);
        truncating.push(b"ab").unwrap();
        truncating.push(b"cd").unwrap();
        assert!(!truncating.truncated, "exactly at the cap is not truncated yet");
        truncating.push(b"").unwrap();
        assert!(truncating.truncated, "any chunk at the cap flips the flag, like remainingBytes <= 0");
        let collected = truncating.finish("~");
        assert_eq!(collected.text, "abcd~");
        assert_eq!(collected.bytes, 4);

        let mut erroring = OutputCollector::new(4, OutputMode::Error);
        erroring.push(b"abcd").unwrap();
        erroring.push(b"").unwrap();
        assert_eq!(erroring.push(b"e"), Err(5));
    }

    #[test]
    fn command_name_drops_directories() {
        assert_eq!(command_name("/usr/bin/git"), "git");
        assert_eq!(command_name("C:\\tools\\gh.exe"), "gh.exe");
        assert_eq!(command_name("git"), "git");
    }
}
