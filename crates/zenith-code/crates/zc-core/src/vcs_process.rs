//! The single spawn point for `git`, `gh`, `glab` and `az` (`apps/server/src/vcs/VcsProcess.ts`).
//!
//! On top of [`crate::process`] it adds:
//! - concurrency limits: **8** VCS processes at once, and at most **4** of them `gh` (a `gh`
//!   call takes its GitHub permit first, then a VCS permit);
//! - defaults: 30 s timeout, 1,000,000-byte cap, [`OutputMode::Truncate`], marker
//!   `"\n\n[truncated]"` only when `append_truncation_marker` is set;
//! - non-zero exits become [`VcsProcessError::Exit`] (unless `allow_non_zero_exit`), classified
//!   from stderr as `authentication | rate-limited | not-found | command-failed`. stderr itself
//!   is dropped from the error (it can hold credentials); only its JS length is kept;
//! - a `retryable` hint for transient git lock/stat races, and up to 2 retries 75 ms apart for
//!   the checkpoint-capture operation.
//!
//! [`VcsProcessError`] serializes exactly like the contracts' `VcsError` members (it crosses the
//! wire in `vcs.*` RPC failures).

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::defect::{js_length, Defect};
use crate::process::{
    ChunkCallback, EnvOverlay, OutputMode, ProcessRunError, ProcessRunInput, ProcessRunner, ProcessStream, SystemProcessRunner, TimeoutBehavior,
};

pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1_000_000;
pub const OUTPUT_TRUNCATED_MARKER: &str = "\n\n[truncated]";
pub const VCS_PROCESS_CONCURRENCY: usize = 8;
pub const GITHUB_PROCESS_CONCURRENCY: usize = 4;
/// The operation name whose transient failures are retried.
pub const CHECKPOINT_CAPTURE_OPERATION: &str = "GitVcsDriver.checkpoints.captureCheckpoint";
const CHECKPOINT_RETRY_TIMES: usize = 2;
const CHECKPOINT_RETRY_SPACING: Duration = Duration::from_millis(75);

/// One VCS CLI invocation (`VcsProcessInput`).
#[derive(Clone, Default)]
pub struct VcsProcessInput {
    /// A stable name for what is being done (`"GitVcsDriver.status"`), reported in errors.
    pub operation: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub spawn_cwd: Option<PathBuf>,
    pub stdin: Option<String>,
    pub on_stdout_chunk: Option<ChunkCallback>,
    pub env: Option<EnvOverlay>,
    pub allow_non_zero_exit: bool,
    /// Defaults to [`DEFAULT_TIMEOUT_MS`].
    pub timeout_ms: Option<u64>,
    /// Defaults to [`DEFAULT_MAX_OUTPUT_BYTES`].
    pub max_output_bytes: Option<usize>,
    /// Defaults to [`OutputMode::Truncate`].
    pub output_mode: Option<OutputMode>,
    pub append_truncation_marker: bool,
}

impl VcsProcessInput {
    pub fn new<I, S>(operation: impl Into<String>, command: impl Into<String>, args: I, cwd: impl Into<PathBuf>) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            operation: operation.into(),
            command: command.into(),
            args: args.into_iter().map(Into::into).collect(),
            cwd: cwd.into(),
            ..Self::default()
        }
    }
}

impl std::fmt::Debug for VcsProcessInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VcsProcessInput")
            .field("operation", &self.operation)
            .field("command", &self.command)
            .field("argument_count", &self.args.len())
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}

/// `VcsProcessOutput`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VcsProcessOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub stdout_invalid_utf8: bool,
    pub stderr_invalid_utf8: bool,
}

/// `VcsProcessExitFailureKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VcsProcessExitFailureKind {
    Authentication,
    NotFound,
    RateLimited,
    CommandFailed,
}

/// Output stream literal of `VcsProcessOutputLimitError` / `VcsProcessOutputReadError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VcsStream {
    #[serde(rename = "stdout")]
    Stdout,
    #[serde(rename = "stderr")]
    Stderr,
    #[serde(rename = "exitCode")]
    ExitCode,
}

impl From<ProcessStream> for VcsStream {
    fn from(stream: ProcessStream) -> Self {
        match stream {
            ProcessStream::Stdout => Self::Stdout,
            ProcessStream::Stderr => Self::Stderr,
            ProcessStream::ExitCode => Self::ExitCode,
        }
    }
}

impl std::fmt::Display for VcsStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::ExitCode => "exitCode",
        })
    }
}

/// The process members of the contracts' `VcsError` union (`packages/contracts/src/vcs.ts`),
/// with the same `_tag`s, field names, optional-key rules and messages.
///
/// To be replaced by (or converted into) `zc_contracts::VcsError` once WP-01 lands; the JSON is
/// already identical.
#[derive(Debug, Clone, PartialEq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "_tag")]
pub enum VcsProcessError {
    #[error("VCS process failed to spawn in {operation}: {command} ({cwd})")]
    #[serde(rename = "VcsProcessSpawnError", rename_all = "camelCase")]
    Spawn {
        operation: String,
        command: String,
        cwd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        argument_count: Option<usize>,
        cause: Defect,
    },
    #[error("VCS process failed in {operation}: {command} ({cwd}) exited with {exit_code} - {detail}")]
    #[serde(rename = "VcsProcessExitError", rename_all = "camelCase")]
    Exit {
        operation: String,
        command: String,
        cwd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        argument_count: Option<usize>,
        exit_code: i32,
        detail: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure_kind: Option<VcsProcessExitFailureKind>,
        /// Present (and `true`) only for a recognized transient failure.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retryable: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stderr_length: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stderr_truncated: Option<bool>,
    },
    #[error("VCS process timed out in {operation}: {command} ({cwd}) after {timeout_ms}ms")]
    #[serde(rename = "VcsProcessTimeoutError", rename_all = "camelCase")]
    Timeout {
        operation: String,
        command: String,
        cwd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        argument_count: Option<usize>,
        timeout_ms: u64,
    },
    #[error("VCS process failed to write {stdin_bytes} bytes to stdin in {operation}: {command} ({cwd})")]
    #[serde(rename = "VcsProcessStdinWriteError", rename_all = "camelCase")]
    StdinWrite {
        operation: String,
        command: String,
        cwd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        argument_count: Option<usize>,
        stdin_bytes: usize,
        cause: Defect,
    },
    #[error("VCS process failed to read {stream} in {operation}: {command} ({cwd})")]
    #[serde(rename = "VcsProcessOutputReadError", rename_all = "camelCase")]
    OutputRead {
        operation: String,
        command: String,
        cwd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        argument_count: Option<usize>,
        stream: VcsStream,
        cause: Defect,
    },
    #[error("VCS process {stream} produced {observed_bytes} bytes in {operation}: {command} ({cwd}), exceeding the {max_bytes} byte limit")]
    #[serde(rename = "VcsProcessOutputLimitError", rename_all = "camelCase")]
    OutputLimit {
        operation: String,
        command: String,
        cwd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        argument_count: Option<usize>,
        stream: VcsStream,
        max_bytes: usize,
        observed_bytes: usize,
    },
    #[error("VCS process completed without an exit code in {operation}: {command} ({cwd})")]
    #[serde(rename = "VcsProcessMissingExitCodeError", rename_all = "camelCase")]
    MissingExitCode {
        operation: String,
        command: String,
        cwd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        argument_count: Option<usize>,
    },
}

impl VcsProcessError {
    /// The `_tag`.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Spawn { .. } => "VcsProcessSpawnError",
            Self::Exit { .. } => "VcsProcessExitError",
            Self::Timeout { .. } => "VcsProcessTimeoutError",
            Self::StdinWrite { .. } => "VcsProcessStdinWriteError",
            Self::OutputRead { .. } => "VcsProcessOutputReadError",
            Self::OutputLimit { .. } => "VcsProcessOutputLimitError",
            Self::MissingExitCode { .. } => "VcsProcessMissingExitCodeError",
        }
    }

    /// True for an exit error flagged as a transient git failure.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Exit { retryable: Some(true), .. })
    }

    /// `VcsProcessExitError.fromProcessExit`.
    pub fn from_process_exit(
        context: &VcsErrorContext,
        exit_code: i32,
        stderr: &str,
        stderr_truncated: bool,
        failure_kind: VcsProcessExitFailureKind,
        retryable: bool,
    ) -> Self {
        let detail = match failure_kind {
            VcsProcessExitFailureKind::Authentication => "Authentication failed.",
            VcsProcessExitFailureKind::RateLimited => "API rate limit exceeded.",
            VcsProcessExitFailureKind::NotFound => match context.command.as_str() {
                "glab" => "Merge request not found.",
                "gh" | "az" => "Pull request not found.",
                _ => "VCS resource not found.",
            },
            VcsProcessExitFailureKind::CommandFailed => "Process exited with a non-zero status.",
        };
        Self::Exit {
            operation: context.operation.clone(),
            command: context.command.clone(),
            cwd: context.cwd.clone(),
            argument_count: Some(context.argument_count),
            exit_code,
            detail: detail.to_owned(),
            failure_kind: Some(failure_kind),
            retryable: retryable.then_some(true),
            stderr_length: Some(js_length(stderr)),
            stderr_truncated: Some(stderr_truncated),
        }
    }
}

/// `VcsProcessErrorContext` (`baseError` in `VcsProcess.ts`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcsErrorContext {
    pub operation: String,
    pub command: String,
    pub cwd: String,
    pub argument_count: usize,
}

/// `classifyNonZeroExit`.
pub fn classify_non_zero_exit(command: &str, stderr: &str) -> VcsProcessExitFailureKind {
    let normalized = stderr.to_lowercase();
    let has = |needle: &str| normalized.contains(needle);
    if [
        "authentication failed",
        "not logged in",
        "gh auth login",
        "glab auth login",
        "az devops login",
        "please run az login",
        "no oauth token",
        "unauthorized",
    ]
    .iter()
    .any(|needle| has(needle))
    {
        return VcsProcessExitFailureKind::Authentication;
    }
    if ["api rate limit", "rate limit exceeded", "secondary rate limit", "too many requests", "http 429"]
        .iter()
        .any(|needle| has(needle))
    {
        return VcsProcessExitFailureKind::RateLimited;
    }
    let not_found = match command {
        "gh" => {
            has("could not resolve to a pullrequest")
                || has("repository.pullrequest")
                || has("no pull requests found for branch")
                || has("pull request not found")
        }
        "glab" => has("merge request not found") || has("not found") || has("404"),
        "az" => has("pull request") && (has("not found") || has("does not exist")),
        _ => false,
    };
    if not_found {
        return VcsProcessExitFailureKind::NotFound;
    }
    VcsProcessExitFailureKind::CommandFailed
}

/// `isTransientGitExit`: a lock-file collision or a vanished path, both of which a retry fixes.
pub fn is_transient_git_exit(stderr: &str) -> bool {
    static LOCK: OnceLock<Regex> = OnceLock::new();
    static STAT: OnceLock<Regex> = OnceLock::new();
    let lock = LOCK.get_or_init(|| Regex::new(r#"(?i)unable to create [^\n]*\.lock['"]?: file exists"#).expect("valid regex"));
    let stat = STAT.get_or_init(|| Regex::new(r"(?i)(?:unable to stat|lstat\(|error: open\()[^\n]+: no such file or directory").expect("valid regex"));
    lock.is_match(stderr) || stat.is_match(stderr)
}

/// The `VcsProcess` service.
#[derive(Clone)]
pub struct VcsProcess {
    runner: Arc<dyn ProcessRunner>,
    vcs_permits: Arc<Semaphore>,
    github_permits: Arc<Semaphore>,
}

impl Default for VcsProcess {
    fn default() -> Self {
        Self::new(Arc::new(SystemProcessRunner))
    }
}

impl VcsProcess {
    pub fn new(runner: Arc<dyn ProcessRunner>) -> Self {
        Self {
            runner,
            vcs_permits: Arc::new(Semaphore::new(VCS_PROCESS_CONCURRENCY)),
            github_permits: Arc::new(Semaphore::new(GITHUB_PROCESS_CONCURRENCY)),
        }
    }

    /// Run one VCS command with the concurrency limits and (for checkpoint capture) retries.
    pub async fn run(&self, input: VcsProcessInput) -> Result<VcsProcessOutput, VcsProcessError> {
        if input.command == "git" && input.operation == CHECKPOINT_CAPTURE_OPERATION && input.on_stdout_chunk.is_none() {
            let mut attempt = 0;
            loop {
                match self.run_bounded(&input).await {
                    Err(error) if error.is_retryable() && attempt < CHECKPOINT_RETRY_TIMES => {
                        tracing::debug!(
                            operation = %input.operation,
                            error_tag = error.tag(),
                            "checkpoint Git command failed"
                        );
                        attempt += 1;
                        tokio::time::sleep(CHECKPOINT_RETRY_SPACING).await;
                    }
                    other => return other,
                }
            }
        }
        if input.command == "gh" {
            let _github = self.github_permits.acquire().await.expect("the GitHub semaphore is never closed");
            return self.run_bounded(&input).await;
        }
        self.run_bounded(&input).await
    }

    async fn run_bounded(&self, input: &VcsProcessInput) -> Result<VcsProcessOutput, VcsProcessError> {
        let _permit = self.vcs_permits.acquire().await.expect("the VCS semaphore is never closed");
        self.run_unbounded(input).await
    }

    async fn run_unbounded(&self, input: &VcsProcessInput) -> Result<VcsProcessOutput, VcsProcessError> {
        let context = VcsErrorContext {
            operation: input.operation.clone(),
            command: input.command.clone(),
            cwd: input.cwd.to_string_lossy().into_owned(),
            argument_count: input.args.len(),
        };
        let run_input = ProcessRunInput {
            command: input.command.clone(),
            args: input.args.clone(),
            cwd: Some(input.cwd.clone()),
            spawn_cwd: input.spawn_cwd.clone(),
            timeout: Some(Duration::from_millis(input.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS))),
            env: input.env.clone(),
            stdin: input.stdin.clone(),
            on_stdout_chunk: input.on_stdout_chunk.clone(),
            max_output_bytes: Some(input.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES)),
            output_mode: input.output_mode.unwrap_or(OutputMode::Truncate),
            truncated_marker: Some(if input.append_truncation_marker {
                OUTPUT_TRUNCATED_MARKER.to_owned()
            } else {
                String::new()
            }),
            timeout_behavior: TimeoutBehavior::Error,
        };
        let result = self.runner.run(run_input).await.map_err(|error| map_process_error(&context, error))?;

        let Some(code) = result.code else {
            return Err(VcsProcessError::MissingExitCode {
                operation: context.operation,
                command: context.command,
                cwd: context.cwd,
                argument_count: Some(context.argument_count),
            });
        };
        if !input.allow_non_zero_exit && code != 0 {
            let failure_kind = classify_non_zero_exit(&input.command, &result.stderr);
            let retryable = input.command == "git" && failure_kind == VcsProcessExitFailureKind::CommandFailed && is_transient_git_exit(&result.stderr);
            return Err(VcsProcessError::from_process_exit(
                &context,
                code,
                &result.stderr,
                result.stderr_truncated,
                failure_kind,
                retryable,
            ));
        }
        Ok(VcsProcessOutput {
            exit_code: code,
            stdout: result.stdout,
            stderr: result.stderr,
            stdout_truncated: result.stdout_truncated,
            stderr_truncated: result.stderr_truncated,
            stdout_invalid_utf8: result.stdout_invalid_utf8,
            stderr_invalid_utf8: result.stderr_invalid_utf8,
        })
    }
}

fn map_process_error(context: &VcsErrorContext, error: ProcessRunError) -> VcsProcessError {
    let operation = context.operation.clone();
    let command = context.command.clone();
    let cwd = context.cwd.clone();
    let argument_count = Some(context.argument_count);
    match error {
        ProcessRunError::Spawn { cause, .. } => VcsProcessError::Spawn {
            operation,
            command,
            cwd,
            argument_count,
            cause,
        },
        ProcessRunError::OutputLimit {
            stream,
            max_bytes,
            observed_bytes,
            ..
        } => VcsProcessError::OutputLimit {
            operation,
            command,
            cwd,
            argument_count,
            stream: stream.into(),
            max_bytes,
            observed_bytes,
        },
        ProcessRunError::Timeout { timeout_ms, .. } => VcsProcessError::Timeout {
            operation,
            command,
            cwd,
            argument_count,
            timeout_ms,
        },
        ProcessRunError::Stdin { stdin_bytes, cause, .. } => VcsProcessError::StdinWrite {
            operation,
            command,
            cwd,
            argument_count,
            stdin_bytes,
            cause,
        },
        ProcessRunError::Read { stream, cause, .. } => VcsProcessError::OutputRead {
            operation,
            command,
            cwd,
            argument_count,
            stream: stream.into(),
            cause,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{ProcessRunOutput, ProcessRunner};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[test]
    fn classifies_stderr_like_ts() {
        use VcsProcessExitFailureKind::*;
        assert_eq!(
            classify_non_zero_exit("gh", "To get started with GitHub CLI, please run:  gh auth login"),
            Authentication
        );
        assert_eq!(classify_non_zero_exit("git", "remote: HTTP 401 Unauthorized"), Authentication);
        assert_eq!(classify_non_zero_exit("gh", "GraphQL: API rate limit exceeded for user"), RateLimited);
        assert_eq!(classify_non_zero_exit("gh", "HTTP 429: Too Many Requests"), RateLimited);
        assert_eq!(
            classify_non_zero_exit("gh", "GraphQL: Could not resolve to a PullRequest with the number of 9"),
            NotFound
        );
        assert_eq!(classify_non_zero_exit("glab", "404 Not Found"), NotFound);
        assert_eq!(classify_non_zero_exit("az", "The pull request does not exist"), NotFound);
        assert_eq!(classify_non_zero_exit("az", "Resource not found"), CommandFailed);
        assert_eq!(classify_non_zero_exit("git", "fatal: not found"), CommandFailed);
    }

    #[test]
    fn recognizes_transient_git_failures() {
        assert!(is_transient_git_exit("fatal: Unable to create '/repo/.git/index.lock': File exists.\n"));
        assert!(is_transient_git_exit("error: unable to stat 'foo/bar': No such file or directory"));
        assert!(is_transient_git_exit("error: open(\"x\"): No such file or directory"));
        assert!(!is_transient_git_exit("fatal: not a git repository"));
    }

    #[test]
    fn exit_error_serializes_like_the_contract() {
        let context = VcsErrorContext {
            operation: "GitVcsDriver.status".into(),
            command: "git".into(),
            cwd: "/repo".into(),
            argument_count: 3,
        };
        let error = VcsProcessError::from_process_exit(&context, 128, "fatal: 😀", false, VcsProcessExitFailureKind::CommandFailed, false);
        assert_eq!(
            serde_json::to_string(&error).unwrap(),
            r#"{"_tag":"VcsProcessExitError","operation":"GitVcsDriver.status","command":"git","cwd":"/repo","argumentCount":3,"exitCode":128,"detail":"Process exited with a non-zero status.","failureKind":"command-failed","stderrLength":9,"stderrTruncated":false}"#
        );
        assert_eq!(
            error.to_string(),
            "VCS process failed in GitVcsDriver.status: git (/repo) exited with 128 - Process exited with a non-zero status."
        );
        let back: VcsProcessError = serde_json::from_str(&serde_json::to_string(&error).unwrap()).unwrap();
        assert_eq!(back, error);
    }

    #[tokio::test]
    async fn non_zero_exit_fails_without_keeping_stderr() {
        let vcs = VcsProcess::default();
        let input = VcsProcessInput::new("test.op", "/bin/sh", ["-c", "printf 'secret token: gh auth login' >&2; exit 1"], "/");
        let error = vcs.run(input).await.unwrap_err();
        assert!(!error.to_string().contains("secret"));
        match error {
            VcsProcessError::Exit {
                failure_kind,
                stderr_length,
                exit_code,
                ..
            } => {
                assert_eq!(failure_kind, Some(VcsProcessExitFailureKind::Authentication));
                assert_eq!(stderr_length, Some(27));
                assert_eq!(exit_code, 1);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn allows_non_zero_exits_and_truncates_with_marker() {
        let vcs = VcsProcess::default();
        let mut input = VcsProcessInput::new("test.op", "/bin/sh", ["-c", "printf 0123456789; exit 2"], "/");
        input.allow_non_zero_exit = true;
        input.max_output_bytes = Some(4);
        input.append_truncation_marker = true;
        let output = vcs.run(input).await.unwrap();
        assert_eq!(output.exit_code, 2);
        assert_eq!(output.stdout, "0123\n\n[truncated]");
        assert!(output.stdout_truncated);
    }

    #[tokio::test]
    async fn timeouts_map_to_the_vcs_timeout_error() {
        let vcs = VcsProcess::default();
        let mut input = VcsProcessInput::new("test.op", "/bin/sleep", ["5"], "/");
        input.timeout_ms = Some(100);
        let error = vcs.run(input).await.unwrap_err();
        assert_eq!(error.tag(), "VcsProcessTimeoutError");
        assert_eq!(serde_json::to_value(&error).unwrap()["timeoutMs"], serde_json::json!(100));
    }

    /// A fake runner that records concurrency and replays scripted results.
    struct FakeRunner {
        running: AtomicUsize,
        peak: AtomicUsize,
        calls: AtomicUsize,
        script: Mutex<Vec<ProcessRunOutput>>,
        hold: Duration,
    }

    #[async_trait]
    impl ProcessRunner for FakeRunner {
        async fn run(&self, _input: ProcessRunInput) -> Result<ProcessRunOutput, ProcessRunError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(self.hold).await;
            self.running.fetch_sub(1, Ordering::SeqCst);
            let mut script = self.script.lock().unwrap();
            Ok(if script.is_empty() {
                ProcessRunOutput {
                    code: Some(0),
                    ..Default::default()
                }
            } else {
                script.remove(0)
            })
        }
    }

    fn fake(hold: Duration, script: Vec<ProcessRunOutput>) -> Arc<FakeRunner> {
        Arc::new(FakeRunner {
            running: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            script: Mutex::new(script),
            hold,
        })
    }

    #[tokio::test]
    async fn bounds_bursts_to_eight_and_gh_to_four() {
        let runner = fake(Duration::from_millis(30), vec![]);
        let vcs = VcsProcess::new(runner.clone());
        let tasks: Vec<_> = (0..24)
            .map(|_| {
                let vcs = vcs.clone();
                tokio::spawn(async move { vcs.run(VcsProcessInput::new("op", "git", ["status"], "/")).await })
            })
            .collect();
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!(runner.peak.load(Ordering::SeqCst), VCS_PROCESS_CONCURRENCY);

        let runner = fake(Duration::from_millis(30), vec![]);
        let vcs = VcsProcess::new(runner.clone());
        let tasks: Vec<_> = (0..16)
            .map(|_| {
                let vcs = vcs.clone();
                tokio::spawn(async move { vcs.run(VcsProcessInput::new("op", "gh", ["api", "user"], "/")).await })
            })
            .collect();
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!(runner.peak.load(Ordering::SeqCst), GITHUB_PROCESS_CONCURRENCY);
    }

    #[tokio::test]
    async fn checkpoint_capture_retries_transient_lock_errors_twice() {
        let lock_failure = || ProcessRunOutput {
            code: Some(128),
            stderr: "fatal: Unable to create '/r/.git/index.lock': File exists.".into(),
            ..Default::default()
        };
        // Two transient failures then success: succeeds on the third attempt.
        let runner = fake(Duration::ZERO, vec![lock_failure(), lock_failure()]);
        let vcs = VcsProcess::new(runner.clone());
        let input = VcsProcessInput::new(CHECKPOINT_CAPTURE_OPERATION, "git", ["write-tree"], "/");
        vcs.run(input.clone()).await.unwrap();
        assert_eq!(runner.calls.load(Ordering::SeqCst), 3);

        // Three transient failures: gives up after 1 + 2 attempts.
        let runner = fake(Duration::ZERO, vec![lock_failure(), lock_failure(), lock_failure()]);
        let vcs = VcsProcess::new(runner.clone());
        let error = vcs.run(input.clone()).await.unwrap_err();
        assert!(error.is_retryable());
        assert_eq!(runner.calls.load(Ordering::SeqCst), 3);

        // Other operations are never retried.
        let runner = fake(Duration::ZERO, vec![lock_failure()]);
        let vcs = VcsProcess::new(runner.clone());
        let error = vcs.run(VcsProcessInput::new("GitVcsDriver.status", "git", ["status"], "/")).await.unwrap_err();
        assert!(error.is_retryable());
        assert_eq!(runner.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn missing_exit_code_is_reported() {
        let runner = fake(Duration::ZERO, vec![ProcessRunOutput::default()]);
        let vcs = VcsProcess::new(runner);
        let error = vcs.run(VcsProcessInput::new("op", "git", ["x"], "/")).await.unwrap_err();
        assert_eq!(error.tag(), "VcsProcessMissingExitCodeError");
    }
}
