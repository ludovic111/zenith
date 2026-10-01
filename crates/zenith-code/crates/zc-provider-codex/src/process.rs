//! Spawning `codex app-server` (Effect's `ChildProcess.make` + `NodeChildProcessSpawner`, as
//! `CodexSessionRuntime.ts` and `CodexProvider.ts` use them).
//!
//! - `env` with `extend_env` adds to the server's environment; without it the child gets only
//!   `env` (TS: `extendEnv = options.environment === undefined`).
//! - The child runs in its own session (`detached: true` on POSIX), so closing it signals the
//!   whole group: `SIGTERM`, then `SIGKILL` after `force_kill_after` (2 s for app-server).
//! - `resolveSpawnCommand` only rewrites commands on Windows; elsewhere the argv is as given.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};

use crate::errors::{CodexAppServerError, TransportOperation};

/// `CODEX_APP_SERVER_FORCE_KILL_AFTER` / `CODEX_APP_SERVER_PROBE_FORCE_KILL_AFTER`.
pub const FORCE_KILL_AFTER: Duration = Duration::from_secs(2);

/// Everything that decides how the child is started; compared with the TS spawn in tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    /// Whether `env` extends the server's environment (else it replaces it).
    pub extend_env: bool,
}

impl SpawnSpec {
    /// `${binaryPath} app-server`, the command named in spawn errors.
    pub fn display_command(&self) -> String {
        format!("{} app-server", self.command)
    }
}

/// A running child with its pipes.
pub struct SpawnedChild {
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
    pub handle: ChildHandle,
}

/// The exit code (`None` when killed by a signal), or why it could not be read.
type ExitStatus = Result<Option<i32>, String>;

/// Owns the process: waits for it, kills it.
#[derive(Clone)]
pub struct ChildHandle {
    pid: Option<u32>,
    exit: Arc<tokio::sync::watch::Sender<Option<ExitStatus>>>,
}

impl std::fmt::Debug for ChildHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildHandle").field("pid", &self.pid).finish_non_exhaustive()
    }
}

/// Starts `spec`. A failure is the TS `CodexAppServerSpawnError` (`command` = `<bin> app-server`).
pub fn spawn(spec: &SpawnSpec) -> Result<SpawnedChild, CodexAppServerError> {
    let mut command = Command::new(&spec.command);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false);
    if !spec.extend_env {
        command.env_clear();
    }
    command.envs(&spec.env);
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
    let mut child = command.spawn().map_err(|error| CodexAppServerError::Spawn {
        command: Some(spec.display_command()),
        cause: error.to_string(),
    })?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let pid = child.id();
    let (exit, _) = tokio::sync::watch::channel(None);
    let handle = ChildHandle { pid, exit: Arc::new(exit) };
    let waiter = handle.clone();
    // The waiter owns the process; killing goes through signals to its pid.
    tokio::spawn(async move {
        let status = child.wait().await.map(|status| status.code()).map_err(|error| error.to_string());
        waiter.exit.send_replace(Some(status));
    });
    Ok(SpawnedChild { stdin, stdout, stderr, handle })
}

impl ChildHandle {
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// The exit code (`None` when killed by a signal), or why it could not be read.
    pub async fn wait(&self) -> Result<Option<i32>, String> {
        let mut receiver = self.exit.subscribe();
        loop {
            if let Some(status) = receiver.borrow_and_update().clone() {
                return status;
            }
            if receiver.changed().await.is_err() {
                return Err("process watcher stopped".to_owned());
            }
        }
    }

    /// `makeTerminationError`: what the protocol reports once stdout ends.
    pub async fn termination_error(&self) -> CodexAppServerError {
        match self.wait().await {
            Ok(code) => CodexAppServerError::ProcessExited { code, pid: self.pid },
            Err(cause) => CodexAppServerError::Transport {
                operation: TransportOperation::ReadProcessExitStatus,
                pid: self.pid,
                cause,
            },
        }
    }

    /// Terminates the process group: `SIGTERM`, then `SIGKILL` after `force_kill_after`.
    pub async fn kill(&self, force_kill_after: Duration) {
        if self.exit.borrow().is_some() {
            return;
        }
        let Some(pid) = self.pid else { return };
        signal(pid, libc::SIGTERM);
        if tokio::time::timeout(force_kill_after, self.wait()).await.is_err() {
            signal(pid, libc::SIGKILL);
            let _ = tokio::time::timeout(Duration::from_secs(1), self.wait()).await;
        }
    }
}

#[cfg(unix)]
fn signal(pid: u32, signal: libc::c_int) {
    #[allow(clippy::cast_possible_wrap)]
    let pid = pid as libc::pid_t;
    // SAFETY: plain syscalls; a negative pid addresses the process group.
    unsafe {
        if libc::kill(-pid, signal) != 0 {
            libc::kill(pid, signal);
        }
    }
}

#[cfg(not(unix))]
fn signal(_pid: u32, _signal: i32) {}
