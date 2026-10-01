//! Running a one-shot CLI the way the TS text generators do with Effect's `ChildProcess`:
//! exactly the given environment (no inheritance), the prompt written to stdin and closed,
//! stdout and stderr collected in full while waiting for the exit code. The caller bounds the
//! run with a timeout; dropping the future kills the child.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// One CLI invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliInvocation {
    pub command: String,
    pub args: Vec<String>,
    /// The child's whole environment.
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    pub stdin: String,
}

/// What a finished CLI left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliOutput {
    pub stdout: String,
    pub stderr: String,
    /// `None` when a signal ended the process.
    pub code: Option<i32>,
}

/// Where a run failed before an exit code was known.
#[derive(Debug)]
pub enum CliError {
    /// The process could not start (`ENOENT` and friends).
    Spawn(std::io::Error),
    /// Reading its output or exit status failed.
    Read(std::io::Error),
}

/// Spawn, feed stdin, collect stdout/stderr, wait.
pub async fn run_cli(invocation: &CliInvocation) -> Result<CliOutput, CliError> {
    let mut command = Command::new(&invocation.command);
    command
        .args(&invocation.args)
        .env_clear()
        .envs(&invocation.env)
        .current_dir(&invocation.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(CliError::Spawn)?;
    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let prompt = invocation.stdin.clone();
    let write = async move {
        if let Some(mut pipe) = stdin.take() {
            // A child that exits without reading stdin closes the pipe: not an error here.
            let _ = pipe.write_all(prompt.as_bytes()).await;
            let _ = pipe.shutdown().await;
        }
    };
    let read_out = async {
        let mut bytes = Vec::new();
        if let Some(pipe) = stdout.as_mut() {
            pipe.read_to_end(&mut bytes).await?;
        }
        Ok::<_, std::io::Error>(bytes)
    };
    let read_err = async {
        let mut bytes = Vec::new();
        if let Some(pipe) = stderr.as_mut() {
            pipe.read_to_end(&mut bytes).await?;
        }
        Ok::<_, std::io::Error>(bytes)
    };
    let ((), out, err, status) = tokio::join!(write, read_out, read_err, child.wait());
    let out = out.map_err(CliError::Read)?;
    let err = err.map_err(CliError::Read)?;
    let status = status.map_err(CliError::Read)?;
    Ok(CliOutput {
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
        code: status.code(),
    })
}

/// `<CLI> command failed: <stderr or stdout>` / `… failed with code N.` for a non-zero exit.
pub fn failed_command_detail(label: &str, output: &CliOutput) -> String {
    let stderr = crate::js::trim(&output.stderr);
    let stdout = crate::js::trim(&output.stdout);
    let detail = if !stderr.is_empty() { stderr } else { stdout };
    if !detail.is_empty() {
        format!("{label} CLI command failed: {detail}")
    } else {
        let code = output.code.map_or_else(|| "null".to_owned(), |code| code.to_string());
        format!("{label} CLI command failed with code {code}.")
    }
}
