//! The login-shell `PATH` fix (`apps/server/src/os-jank.ts` `fixPath`, helpers from
//! `packages/shared/src/shell.ts`).
//!
//! A server started from a GUI (launchd, the dashboard app) inherits a minimal `PATH` that lacks
//! Homebrew, nvm, `~/.local/bin` and friends, so `claude`, `codex`, `gh` would not be found. At
//! startup the server therefore asks the user's login shell for its `PATH`:
//!
//! ```text
//! <shell> -ilc "printf '%s\n' '__T3CODE_ENV_PATH_START__'; printenv PATH || true; printf '%s\n' '__T3CODE_ENV_PATH_END__'"
//! ```
//!
//! with a 5 s timeout, taking what lies between the markers (rc files may print anything around
//! them). Candidate shells are `$SHELL`, the passwd shell, then `/bin/zsh` (macOS) or `/bin/bash`
//! (Linux), deduplicated. On macOS, when no shell answers, `launchctl getenv PATH` (2 s) is the
//! fallback. The result is merged in front of the inherited `PATH` (dedupe, trim, drop empties).
//! `HOME` is filled from the passwd entry when empty. Failures are warnings on stderr, never
//! fatal.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use regex::Regex;

/// `execFileSync` timeout for the login shell.
pub const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_millis(5_000);
/// `execFileSync` timeout for `launchctl getenv PATH`.
pub const LAUNCHCTL_TIMEOUT: Duration = Duration::from_millis(2_000);
/// `execFileSync`'s default `maxBuffer` (1 MiB); more output is an error.
const EXEC_MAX_BUFFER: usize = 1024 * 1024;

/// Host platforms the fix distinguishes (`process.platform`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Darwin,
    Linux,
    Windows,
    Other,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Darwin
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Other
        }
    }

    fn path_delimiter(self) -> char {
        if self == Self::Windows {
            ';'
        } else {
            ':'
        }
    }
}

/// `__T3CODE_ENV_<NAME>_START__`.
pub fn env_capture_start(name: &str) -> String {
    format!("__T3CODE_ENV_{name}_START__")
}

/// `__T3CODE_ENV_<NAME>_END__`.
pub fn env_capture_end(name: &str) -> String {
    format!("__T3CODE_ENV_{name}_END__")
}

/// Why a login-shell probe failed.
#[derive(Debug, thiserror::Error)]
pub enum ShellEnvError {
    #[error("Unsupported environment variable name: {0}")]
    UnsupportedName(String),
    #[error("failed to run {program}: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{program} timed out after {}ms", .timeout.as_millis())]
    Timeout { program: String, timeout: Duration },
    #[error("{program} exited with {status}")]
    Exit { program: String, status: String },
    #[error("{program} produced more than {EXEC_MAX_BUFFER} bytes")]
    MaxBuffer { program: String },
}

fn env_name_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Z0-9_]+$").expect("valid regex"))
}

/// `buildEnvironmentCaptureCommand`.
pub fn build_environment_capture_command(names: &[&str]) -> Result<String, ShellEnvError> {
    names
        .iter()
        .map(|name| {
            if !env_name_regex().is_match(name) {
                return Err(ShellEnvError::UnsupportedName((*name).to_owned()));
            }
            Ok(format!(
                "printf '%s\\n' '{}'; printenv {name} || true; printf '%s\\n' '{}'",
                env_capture_start(name),
                env_capture_end(name)
            ))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|parts| parts.join("; "))
}

/// `extractEnvironmentValue`: the text between the markers minus one leading and one trailing
/// line break; `None` when a marker is missing or the value is empty.
pub fn extract_environment_value(output: &str, name: &str) -> Option<String> {
    let start_marker = env_capture_start(name);
    let end_marker = env_capture_end(name);
    let start = output.find(&start_marker)?;
    let value_start = start + start_marker.len();
    let end = value_start + output[value_start..].find(&end_marker)?;
    let mut value = &output[value_start..end];
    if let Some(rest) = value.strip_prefix("\r\n").or_else(|| value.strip_prefix('\n')) {
        value = rest;
    }
    if let Some(rest) = value.strip_suffix("\r\n").or_else(|| value.strip_suffix('\n')) {
        value = rest;
    }
    (!value.is_empty()).then(|| value.to_owned())
}

/// `listLoginShellCandidates`: `$SHELL`, the passwd shell, then the platform fallback, trimmed and
/// deduplicated.
pub fn list_login_shell_candidates(platform: Platform, shell: Option<&str>, user_shell: Option<&str>) -> Vec<String> {
    let fallback = match platform {
        Platform::Darwin => Some("/bin/zsh"),
        Platform::Linux => Some("/bin/bash"),
        _ => None,
    };
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    for candidate in [shell.map(str::trim), user_shell.map(str::trim), fallback] {
        let Some(candidate) = candidate.filter(|c| !c.is_empty()) else {
            continue;
        };
        if seen.insert(candidate.to_owned()) {
            candidates.push(candidate.to_owned());
        }
    }
    candidates
}

/// `mergePathEntries`: entries of `preferred` then `inherited`, trimmed, empties and duplicates
/// dropped; `None` when nothing is left.
pub fn merge_path_entries(preferred: Option<&str>, inherited: Option<&str>, platform: Platform) -> Option<String> {
    let delimiter = platform.path_delimiter();
    let mut seen = HashSet::new();
    let mut merged: Vec<&str> = Vec::new();
    for value in [preferred, inherited].into_iter().flatten() {
        for entry in value.split(delimiter) {
            let entry = entry.trim();
            if entry.is_empty() || !seen.insert(entry) {
                continue;
            }
            merged.push(entry);
        }
    }
    (!merged.is_empty()).then(|| merged.join(&delimiter.to_string()))
}

/// Run a program like `execFileSync(file, args, { encoding: "utf8", timeout })`: stdout is
/// returned, stderr goes to ours, a non-zero exit, a signal, a timeout (the child gets `SIGTERM`)
/// or more than 1 MiB of output is an error.
pub fn exec_file_sync(program: &str, args: &[&str], timeout: Duration) -> Result<String, ShellEnvError> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|source| ShellEnvError::Spawn {
            program: program.to_owned(),
            source,
        })?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buffer.extend_from_slice(&chunk[..n]);
                    if buffer.len() > EXEC_MAX_BUFFER {
                        break;
                    }
                }
            }
        }
        let _ = sender.send(buffer);
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                #[cfg(unix)]
                // SAFETY: plain syscall on our own child.
                unsafe {
                    libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
                }
                #[cfg(not(unix))]
                let _ = child.kill();
                let _ = child.wait();
                return Err(ShellEnvError::Timeout {
                    program: program.to_owned(),
                    timeout,
                });
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(source) => {
                return Err(ShellEnvError::Spawn {
                    program: program.to_owned(),
                    source,
                })
            }
        }
    };
    // A grandchild can keep stdout open after the shell exits: wait no longer than the deadline.
    let remaining = deadline.saturating_duration_since(Instant::now());
    let output = match receiver.recv_timeout(remaining.max(Duration::from_millis(50))) {
        Ok(output) => output,
        Err(_) => {
            return Err(ShellEnvError::Timeout {
                program: program.to_owned(),
                timeout,
            })
        }
    };
    if output.len() > EXEC_MAX_BUFFER {
        return Err(ShellEnvError::MaxBuffer { program: program.to_owned() });
    }
    if !status.success() {
        return Err(ShellEnvError::Exit {
            program: program.to_owned(),
            status: status.to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
}

/// `readEnvironmentFromLoginShell`: run `<shell> -ilc <capture>` and pick the marked values.
pub fn read_environment_from_login_shell(shell: &str, names: &[&str]) -> Result<BTreeMap<String, String>, ShellEnvError> {
    if names.is_empty() {
        return Ok(BTreeMap::new());
    }
    let command = build_environment_capture_command(names)?;
    let output = exec_file_sync(shell, &["-ilc", &command], LOGIN_SHELL_TIMEOUT)?;
    Ok(names
        .iter()
        .filter_map(|name| extract_environment_value(&output, name).map(|value| ((*name).to_owned(), value)))
        .collect())
}

/// `readPathFromLoginShell`.
pub fn read_path_from_login_shell(shell: &str) -> Result<Option<String>, ShellEnvError> {
    Ok(read_environment_from_login_shell(shell, &["PATH"])?.remove("PATH"))
}

/// `readPathFromLaunchctl`: `launchctl getenv PATH`, trimmed; `None` on any failure.
pub fn read_path_from_launchctl() -> Option<String> {
    exec_file_sync("/bin/launchctl", &["getenv", "PATH"], LAUNCHCTL_TIMEOUT)
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// What [`fix_path`] decided. Apply it with [`PathFix::apply_to_process`] (or use the values
/// when building child environments).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathFix {
    /// The new `PATH`, when it changed or was missing.
    pub path: Option<String>,
    /// The new `HOME`, when it was empty.
    pub home: Option<String>,
    /// Warnings in the TS format (`[server] …`), already written to stderr.
    pub warnings: Vec<String>,
}

impl PathFix {
    /// Set the variables in this process. Call it at the very start of `main`, before the async
    /// runtime or any other thread starts: changing the environment is not thread-safe.
    pub fn apply_to_process(&self) {
        if let Some(home) = &self.home {
            std::env::set_var("HOME", home);
        }
        if let Some(path) = &self.path {
            std::env::set_var("PATH", path);
        }
    }
}

/// Probes used by [`fix_path_with`]; swapped out in tests.
pub struct PathProbes<'a> {
    pub user_shell: Option<String>,
    pub user_home: Option<String>,
    pub read_login_shell_path: &'a dyn Fn(&str) -> Result<Option<String>, ShellEnvError>,
    pub read_launchctl_path: &'a dyn Fn() -> Option<String>,
}

/// `fixPath` for this machine: reads `SHELL`, `HOME` and `PATH` from the process environment.
/// Windows (`resolveWindowsEnvironment`) is not ported: it returns no change there.
pub fn fix_path() -> PathFix {
    let env = |name: &str| std::env::var(name).ok();
    let probes = PathProbes {
        user_shell: crate::paths::passwd_shell(),
        user_home: crate::paths::passwd_home_dir().map(|p| p.to_string_lossy().into_owned()),
        read_login_shell_path: &read_path_from_login_shell,
        read_launchctl_path: &read_path_from_launchctl,
    };
    let fix = fix_path_with(
        Platform::current(),
        env("SHELL").as_deref(),
        env("HOME").as_deref(),
        env("PATH").as_deref(),
        &probes,
    );
    for warning in &fix.warnings {
        eprintln!("{warning}");
    }
    fix
}

/// The pure decision of `fixPath` (`hydratePosixHome` + `hydratePosixPath`).
pub fn fix_path_with(platform: Platform, shell: Option<&str>, home: Option<&str>, inherited_path: Option<&str>, probes: &PathProbes<'_>) -> PathFix {
    let mut fix = PathFix::default();
    if platform != Platform::Darwin && platform != Platform::Linux {
        return fix;
    }
    if home.map(str::trim).unwrap_or_default().is_empty() {
        match probes.user_home.as_deref().filter(|h| !h.is_empty()) {
            Some(user_home) => fix.home = Some(user_home.to_owned()),
            None => fix.warnings.push("[server] Failed to hydrate HOME from the user account. ".to_owned()),
        }
    }
    let mut shell_path = None;
    for candidate in list_login_shell_candidates(platform, shell, probes.user_shell.as_deref()) {
        match (probes.read_login_shell_path)(&candidate) {
            Ok(path) => shell_path = path,
            Err(error) => fix.warnings.push(format!("[server] Failed to read PATH from login shell {candidate}. {error}")),
        }
        if shell_path.is_some() {
            break;
        }
    }
    let launchctl_path = if platform == Platform::Darwin && shell_path.is_none() {
        (probes.read_launchctl_path)()
    } else {
        None
    };
    let merged = merge_path_entries(shell_path.as_deref().or(launchctl_path.as_deref()), inherited_path, platform);
    if let Some(merged) = merged {
        if Some(merged.as_str()) != inherited_path {
            fix.path = Some(merged);
        }
    }
    fix
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_the_marker_command() {
        assert_eq!(
            build_environment_capture_command(&["PATH"]).unwrap(),
            "printf '%s\\n' '__T3CODE_ENV_PATH_START__'; printenv PATH || true; printf '%s\\n' '__T3CODE_ENV_PATH_END__'"
        );
        assert!(build_environment_capture_command(&["PATH; rm -rf /"]).is_err());
        assert!(build_environment_capture_command(&["path"]).is_err());
    }

    #[test]
    fn extracts_values_between_noisy_output() {
        let output = "Welcome to fish-like zsh!\n__T3CODE_ENV_PATH_START__\n/opt/homebrew/bin:/usr/bin\n__T3CODE_ENV_PATH_END__\nbye\n";
        assert_eq!(extract_environment_value(output, "PATH").as_deref(), Some("/opt/homebrew/bin:/usr/bin"));
        let crlf = "__T3CODE_ENV_PATH_START__\r\n/a:/b\r\n__T3CODE_ENV_PATH_END__";
        assert_eq!(extract_environment_value(crlf, "PATH").as_deref(), Some("/a:/b"));
        assert_eq!(extract_environment_value("__T3CODE_ENV_PATH_START__\n__T3CODE_ENV_PATH_END__", "PATH"), None);
        assert_eq!(extract_environment_value("__T3CODE_ENV_PATH_START__\n/a", "PATH"), None);
        assert_eq!(extract_environment_value("nothing", "PATH"), None);
    }

    #[test]
    fn lists_candidates_in_order_without_duplicates() {
        assert_eq!(
            list_login_shell_candidates(Platform::Darwin, Some(" /bin/zsh "), Some("/bin/zsh")),
            vec!["/bin/zsh".to_owned()]
        );
        assert_eq!(
            list_login_shell_candidates(Platform::Linux, Some("/usr/bin/fish"), Some("/bin/zsh")),
            vec!["/usr/bin/fish".to_owned(), "/bin/zsh".to_owned(), "/bin/bash".to_owned()]
        );
        assert_eq!(list_login_shell_candidates(Platform::Windows, None, Some("")), Vec::<String>::new());
    }

    #[test]
    fn merges_paths_preferring_the_shell() {
        assert_eq!(
            merge_path_entries(Some("/opt/homebrew/bin: /usr/bin::"), Some("/usr/bin:/bin"), Platform::Darwin).as_deref(),
            Some("/opt/homebrew/bin:/usr/bin:/bin")
        );
        assert_eq!(merge_path_entries(None, None, Platform::Darwin), None);
        assert_eq!(
            merge_path_entries(Some("C:\\a;C:\\b"), Some("C:\\a"), Platform::Windows).as_deref(),
            Some("C:\\a;C:\\b")
        );
    }

    #[test]
    fn falls_back_through_shells_then_launchctl() {
        let calls = std::cell::RefCell::new(Vec::new());
        let read_shell = |shell: &str| -> Result<Option<String>, ShellEnvError> {
            calls.borrow_mut().push(shell.to_owned());
            if shell == "/bin/broken" {
                Err(ShellEnvError::Timeout {
                    program: shell.to_owned(),
                    timeout: LOGIN_SHELL_TIMEOUT,
                })
            } else {
                Ok(None)
            }
        };
        let launchctl = || Some("/from/launchctl".to_owned());
        let probes = PathProbes {
            user_shell: Some("/bin/broken".into()),
            user_home: Some("/Users/ada".into()),
            read_login_shell_path: &read_shell,
            read_launchctl_path: &launchctl,
        };
        let fix = fix_path_with(Platform::Darwin, None, Some(""), Some("/usr/bin"), &probes);
        assert_eq!(*calls.borrow(), vec!["/bin/broken".to_owned(), "/bin/zsh".to_owned()]);
        assert_eq!(fix.path.as_deref(), Some("/from/launchctl:/usr/bin"));
        assert_eq!(fix.home.as_deref(), Some("/Users/ada"));
        assert_eq!(fix.warnings.len(), 1);
        assert!(fix.warnings[0].starts_with("[server] Failed to read PATH from login shell /bin/broken."));

        // Linux never asks launchctl, and an unchanged PATH is not reported.
        let fix = fix_path_with(Platform::Linux, Some("/bin/sh"), Some("/home/x"), Some("/usr/bin"), &probes);
        assert_eq!(fix.path, None);
        assert_eq!(fix.home, None);
        // Other platforms are left alone.
        assert_eq!(fix_path_with(Platform::Windows, None, None, None, &probes), PathFix::default());
    }

    #[cfg(unix)]
    #[test]
    fn reads_path_from_a_real_shell() {
        // `sh -ilc` works everywhere; the value must be the PATH the shell sees.
        let path = read_path_from_login_shell("/bin/sh").unwrap();
        assert!(path.is_some_and(|p| !p.is_empty()));
    }

    #[cfg(unix)]
    #[test]
    fn exec_file_sync_times_out_and_reports_failures() {
        let started = Instant::now();
        let error = exec_file_sync("/bin/sleep", &["5"], Duration::from_millis(100)).unwrap_err();
        assert!(matches!(error, ShellEnvError::Timeout { .. }));
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(matches!(
            exec_file_sync("/bin/sh", &["-c", "exit 3"], LOGIN_SHELL_TIMEOUT),
            Err(ShellEnvError::Exit { .. })
        ));
        assert!(matches!(
            exec_file_sync("/no/such/shell", &[], LOGIN_SHELL_TIMEOUT),
            Err(ShellEnvError::Spawn { .. })
        ));
        assert_eq!(exec_file_sync("/bin/echo", &["hi"], LOGIN_SHELL_TIMEOUT).unwrap(), "hi\n");
    }
}
