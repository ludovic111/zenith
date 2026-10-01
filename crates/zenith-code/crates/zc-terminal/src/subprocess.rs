//! Is a terminal busy? (`terminal/Manager.ts`: the process-table snapshot, its `ps` /
//! PowerShell fallbacks, `deriveSubprocessInspectResult`, `subprocessSnapshotPollDelayMs`).
//!
//! One process table is read per poll tick and shared by every terminal (per-terminal
//! `pgrep` calls exhausted the pid space on busy hosts, upstream #6332). The table comes from
//! the resource monitor when the server has one ([`ProcessTableSource`]), else from
//! `ps -eo pid=,ppid=,comm=` (PowerShell `Get-CimInstance` on Windows). A failed or partial
//! table is not authoritative: the tick is skipped rather than marking every terminal idle.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use zc_core::process::{OutputMode, TimeoutBehavior};
use zc_core::shell_env::Platform;
use zc_core::{ProcessRunInput, ProcessRunner};

use crate::shell::{normalize_child_command_name, truncate_terminal_wire_label};

/// `DEFAULT_SUBPROCESS_POLL_INTERVAL_MS`.
pub const DEFAULT_SUBPROCESS_POLL_INTERVAL: Duration = Duration::from_millis(1_000);
/// `MAX_SUBPROCESS_POLL_INTERVAL_MS`.
pub const MAX_SUBPROCESS_POLL_INTERVAL: Duration = Duration::from_millis(60_000);

/// One row of the resource monitor's process table (`ResourceMonitorProcessTableEntry`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessTableEntry {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
}

/// `TerminalSubprocessInspectResult`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubprocessInspectResult {
    pub has_running_subprocess: bool,
    pub child_command: Option<String>,
    /// The shell and every descendant (for port discovery); empty when idle.
    pub process_ids: Vec<u32>,
}

/// `TerminalSubprocessCheckError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubprocessCheckError {
    /// `powershell`, `ps` or `resource-monitor`.
    pub command: &'static str,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub cause: Option<String>,
}

impl SubprocessCheckError {
    pub fn new(command: &'static str, cause: impl Into<String>) -> Self {
        Self {
            command,
            exit_code: None,
            timed_out: false,
            stdout_truncated: false,
            cause: Some(cause.into()),
        }
    }
}

impl std::fmt::Display for SubprocessCheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut details = Vec::new();
        if let Some(code) = self.exit_code {
            details.push(format!("exit code {code}"));
        }
        if self.timed_out {
            details.push("timed out".to_owned());
        }
        if self.stdout_truncated {
            details.push("output truncated".to_owned());
        }
        write!(f, "Failed to inspect terminal subprocesses with {}", self.command)?;
        if !details.is_empty() {
            write!(f, " ({})", details.join(", "))?;
        }
        Ok(())
    }
}

impl std::error::Error for SubprocessCheckError {}

/// Where the process table comes from when the server has a resource monitor
/// (`nativeTelemetry.processTable`).
pub type ProcessTableSource = Arc<dyn Fn() -> BoxFuture<'static, Result<Vec<ProcessTableEntry>, SubprocessCheckError>> + Send + Sync>;

/// A per-terminal inspector, replacing the table entirely (the TS test seam
/// `subprocessInspector`).
pub type SubprocessInspector = Arc<dyn Fn(u32) -> BoxFuture<'static, Result<SubprocessInspectResult, SubprocessCheckError>> + Send + Sync>;

/// `TerminalProcessTableSnapshot`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessTableSnapshot {
    pub children_by_parent: HashMap<u32, Vec<u32>>,
    pub command_by_id: HashMap<u32, String>,
}

/// `subprocessSnapshotPollDelayMs`: the interval doubled per consecutive failure, capped at
/// 60 s.
pub fn subprocess_snapshot_poll_delay(interval: Duration, failure_count: u32) -> Duration {
    let factor = 2u64.saturating_pow(failure_count);
    let millis = (interval.as_millis() as u64).saturating_mul(factor);
    Duration::from_millis(millis.min(MAX_SUBPROCESS_POLL_INTERVAL.as_millis() as u64))
}

/// `parsePosixProcessTable`: `pid ppid comm` lines (`comm` may contain spaces).
pub fn parse_posix_process_table(stdout: &str) -> ProcessTableSnapshot {
    let mut snapshot = ProcessTableSnapshot::default();
    for line in stdout.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let rest = line.trim_start();
        let Some((pid, rest)) = split_number(rest) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some((ppid, rest)) = split_number(rest) else {
            continue;
        };
        // `\s+(.+)$`: at least one blank, then at least one character.
        if !rest.starts_with(char::is_whitespace) || rest.chars().count() < 2 {
            continue;
        }
        let command = rest.trim();
        snapshot.command_by_id.insert(pid, command.to_owned());
        snapshot.children_by_parent.entry(ppid).or_default().push(pid);
    }
    snapshot
}

fn split_number(text: &str) -> Option<(u32, &str)> {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let value = text[..digits].parse().ok()?;
    Some((value, &text[digits..]))
}

/// `processTableSnapshotFromProcesses`.
pub fn snapshot_from_entries(entries: &[ProcessTableEntry]) -> ProcessTableSnapshot {
    let mut snapshot = ProcessTableSnapshot::default();
    for entry in entries {
        snapshot.command_by_id.insert(entry.pid, entry.name.trim().to_owned());
        snapshot.children_by_parent.entry(entry.ppid).or_default().push(entry.pid);
    }
    snapshot
}

/// The `pid|ppid|name` lines of the Windows fallback.
pub fn parse_windows_process_table(stdout: &str) -> ProcessTableSnapshot {
    let entries: Vec<ProcessTableEntry> = stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.trim().splitn(3, '|');
            let pid: u32 = parts.next()?.parse().ok()?;
            let ppid: u32 = parts.next()?.parse().ok()?;
            let name = parts.next().unwrap_or("").to_owned();
            (pid > 0).then_some(ProcessTableEntry { pid, ppid, name })
        })
        .collect();
    snapshot_from_entries(&entries)
}

/// `deriveSubprocessInspectResult`: the terminal is busy when its shell has a child, except a
/// childless copy of the shell itself (an async prompt worker).
pub fn derive_subprocess_inspect_result(snapshot: &ProcessTableSnapshot, terminal_pid: u32, platform: Platform) -> SubprocessInspectResult {
    let command_name = |pid: u32| normalize_child_command_name(snapshot.command_by_id.get(&pid).map_or("", String::as_str), platform);
    let shell_name = command_name(terminal_pid);
    let children = snapshot.children_by_parent.get(&terminal_pid).cloned().unwrap_or_default();
    let child_pid = children.into_iter().find(|&pid| {
        shell_name.is_none() || command_name(pid) != shell_name || snapshot.children_by_parent.get(&pid).is_some_and(|grandchildren| !grandchildren.is_empty())
    });
    let Some(child_pid) = child_pid else {
        return SubprocessInspectResult::default();
    };
    let mut process_ids = vec![terminal_pid];
    let mut seen: HashSet<u32> = HashSet::from([terminal_pid]);
    let mut pending = vec![terminal_pid];
    while let Some(parent) = pending.pop() {
        for &pid in snapshot.children_by_parent.get(&parent).into_iter().flatten() {
            if seen.insert(pid) {
                process_ids.push(pid);
                pending.push(pid);
            }
        }
    }
    SubprocessInspectResult {
        has_running_subprocess: true,
        child_command: command_name(child_pid).map(|name| truncate_terminal_wire_label(&name)),
        process_ids,
    }
}

/// `resolvePosixPsCommand`: an absolute `ps`, so each tick does not walk `PATH`.
pub fn resolve_posix_ps_command() -> String {
    ["/bin/ps", "/usr/bin/ps"]
        .into_iter()
        .find(|candidate| Path::new(candidate).exists())
        .unwrap_or("ps")
        .to_owned()
}

/// `posixProcessTableSnapshot` / `windowsProcessTableSnapshot` through the process runner.
pub async fn fallback_process_table(runner: &dyn ProcessRunner, platform: Platform, ps_command: &str) -> Result<ProcessTableSnapshot, SubprocessCheckError> {
    let (label, mut input, max_bytes, timeout) = if platform == Platform::Windows {
        let command = "Get-CimInstance Win32_Process -ErrorAction Stop | ForEach-Object { Write-Output \"$($_.ProcessId)|$($_.ParentProcessId)|$($_.Name)\" }";
        (
            "powershell",
            ProcessRunInput::new("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", command]),
            262_144,
            Duration::from_millis(1_500),
        )
    } else {
        (
            "ps",
            ProcessRunInput::new(ps_command, ["-eo", "pid=,ppid=,comm="]),
            524_288,
            Duration::from_secs(1),
        )
    };
    input.timeout = Some(timeout);
    input.max_output_bytes = Some(max_bytes);
    input.output_mode = OutputMode::Truncate;
    input.timeout_behavior = TimeoutBehavior::TimedOutResult;
    let output = runner.run(input).await.map_err(|error| SubprocessCheckError::new(label, error.to_string()))?;
    if output.code != Some(0) || output.timed_out || output.stdout_truncated {
        return Err(SubprocessCheckError {
            command: label,
            exit_code: output.code,
            timed_out: output.timed_out,
            stdout_truncated: output.stdout_truncated,
            cause: None,
        });
    }
    Ok(if platform == Platform::Windows {
        parse_windows_process_table(&output.stdout)
    } else {
        parse_posix_process_table(&output.stdout)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_delays() {
        let second = Duration::from_millis(1_000);
        assert_eq!(subprocess_snapshot_poll_delay(second, 0), Duration::from_millis(1_000));
        assert_eq!(subprocess_snapshot_poll_delay(second, 1), Duration::from_millis(2_000));
        assert_eq!(subprocess_snapshot_poll_delay(second, 2), Duration::from_millis(4_000));
        assert_eq!(subprocess_snapshot_poll_delay(second, 30), Duration::from_millis(60_000));
    }

    #[test]
    fn parses_ps_output_with_spaces_in_commands() {
        let snapshot = parse_posix_process_table("  100  9000 vim\n  101   100 git\r\n  200  9001 /usr/bin/python3\nbad line\n  300  1 Google Chrome Helper\n");
        assert_eq!(snapshot.command_by_id[&300], "Google Chrome Helper");
        assert_eq!(snapshot.children_by_parent[&9000], vec![100]);
        let vim = derive_subprocess_inspect_result(&snapshot, 9000, Platform::Linux);
        assert!(vim.has_running_subprocess);
        assert_eq!(vim.child_command.as_deref(), Some("vim"));
        let mut ids = vim.process_ids.clone();
        ids.sort();
        assert_eq!(ids, vec![100, 101, 9000]);
        let python = derive_subprocess_inspect_result(&snapshot, 9001, Platform::Linux);
        assert_eq!(python.child_command.as_deref(), Some("python3"));
        assert_eq!(
            derive_subprocess_inspect_result(&snapshot, 4242, Platform::Linux),
            SubprocessInspectResult::default()
        );
    }

    #[test]
    fn ignores_a_childless_copy_of_the_shell() {
        let snapshot = snapshot_from_entries(&[
            ProcessTableEntry {
                pid: 9000,
                ppid: 1,
                name: "zsh".into(),
            },
            ProcessTableEntry {
                pid: 100,
                ppid: 9000,
                name: "zsh".into(),
            },
            ProcessTableEntry {
                pid: 9002,
                ppid: 1,
                name: "zsh".into(),
            },
            ProcessTableEntry {
                pid: 300,
                ppid: 9002,
                name: "zsh".into(),
            },
            ProcessTableEntry {
                pid: 301,
                ppid: 300,
                name: "sleep".into(),
            },
        ]);
        assert!(!derive_subprocess_inspect_result(&snapshot, 9000, Platform::Linux).has_running_subprocess);
        let subshell = derive_subprocess_inspect_result(&snapshot, 9002, Platform::Linux);
        assert!(subshell.has_running_subprocess);
        assert_eq!(subshell.child_command.as_deref(), Some("zsh"));
        let windows = parse_windows_process_table("100|9000|ping.exe\r\n0|0|Idle\nx|y|z\n");
        assert_eq!(
            derive_subprocess_inspect_result(&windows, 9000, Platform::Windows).child_command.as_deref(),
            Some("ping")
        );
    }
}
