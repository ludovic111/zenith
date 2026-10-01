//! Which shell a terminal runs and with which environment (`terminal/Manager.ts`:
//! `resolveShellCandidates`, `isRetryableShellSpawnError`, `createTerminalSpawnEnv`,
//! `stripAppImageRuntimeEnv`, `normalizedRuntimeEnv`), plus `getTerminalLabel` of
//! `packages/shared/src/terminalLabels.ts`.

use std::collections::BTreeMap;

use zc_core::defect::js_length;
use zc_core::shell_env::Platform;

use crate::pty::PtySpawnError;

/// A shell and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellCandidate {
    pub shell: String,
    pub args: Vec<String>,
}

impl ShellCandidate {
    /// `formatShellCandidate`.
    pub fn format(&self) -> String {
        if self.args.is_empty() {
            self.shell.clone()
        } else {
            format!("{} {}", self.shell, self.args.join(" "))
        }
    }
}

/// `defaultShellResolver`: `$SHELL`, else `bash` (`pwsh.exe` on Windows).
pub fn default_shell(platform: Platform, env: &BTreeMap<String, String>) -> String {
    if platform == Platform::Windows {
        return "pwsh.exe".into();
    }
    env.get("SHELL").cloned().unwrap_or_else(|| "bash".into())
}

/// `normalizeShellCommand`: the first word, unquoted (the whole trimmed value on Windows).
pub fn normalize_shell_command(value: Option<&str>, platform: Platform) -> Option<String> {
    let trimmed = value?.trim();
    if trimmed.is_empty() {
        return None;
    }
    if platform == Platform::Windows {
        return Some(trimmed.to_owned());
    }
    let first = trimmed.split_whitespace().next()?.trim();
    if first.is_empty() {
        return None;
    }
    let first = first.strip_prefix(['\'', '"']).unwrap_or(first);
    let first = first.strip_suffix(['\'', '"']).unwrap_or(first);
    Some(first.to_owned())
}

fn basename_for_platform(command: &str, platform: Platform) -> String {
    let normalized = if platform == Platform::Windows {
        command.replace('/', "\\")
    } else {
        command.replace('\\', "/")
    };
    let separator = if platform == Platform::Windows { '\\' } else { '/' };
    normalized
        .split(separator)
        .rfind(|part| !part.is_empty())
        .map(str::to_owned)
        .unwrap_or(normalized)
}

/// `shellCandidateFromCommand`: zsh gets `-o nopromptsp` (no `%` end-of-output markers),
/// PowerShell `-NoLogo`.
pub fn shell_candidate_from_command(command: Option<&str>, platform: Platform) -> Option<ShellCandidate> {
    let command = command.filter(|c| !c.is_empty())?;
    let name = basename_for_platform(command, platform).to_lowercase();
    let args: Vec<String> = if platform == Platform::Windows && (name == "pwsh.exe" || name == "powershell.exe") {
        vec!["-NoLogo".into()]
    } else if platform != Platform::Windows && name == "zsh" {
        vec!["-o".into(), "nopromptsp".into()]
    } else {
        Vec::new()
    };
    Some(ShellCandidate {
        shell: command.to_owned(),
        args,
    })
}

fn join_windows_path(parts: &[&str]) -> String {
    parts
        .iter()
        .enumerate()
        .map(|(index, part)| {
            if index == 0 {
                part.trim_end_matches(['\\', '/']).to_owned()
            } else {
                part.trim_matches(['\\', '/']).to_owned()
            }
        })
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\\")
}

fn windows_system_root(env: &BTreeMap<String, String>) -> String {
    for key in ["SystemRoot", "windir"] {
        if let Some(value) = env.get(key).map(|v| v.trim()).filter(|v| !v.is_empty()) {
            return value.to_owned();
        }
    }
    "C:\\Windows".into()
}

/// `resolveShellCandidates`: the requested shell first, then the platform fallbacks, without
/// duplicates.
pub fn resolve_shell_candidates(requested: &str, platform: Platform, env: &BTreeMap<String, String>) -> Vec<ShellCandidate> {
    let candidate = |command: Option<String>| shell_candidate_from_command(command.as_deref(), platform);
    let requested = candidate(normalize_shell_command(Some(requested), platform));
    let list: Vec<Option<ShellCandidate>> = if platform == Platform::Windows {
        let root = windows_system_root(env);
        vec![
            requested,
            candidate(Some("pwsh.exe".into())),
            candidate(Some(join_windows_path(&[&root, "System32", "WindowsPowerShell", "v1.0", "powershell.exe"]))),
            candidate(Some("powershell.exe".into())),
            candidate(env.get("ComSpec").cloned()),
            candidate(Some(join_windows_path(&[&root, "System32", "cmd.exe"]))),
            candidate(Some("cmd.exe".into())),
        ]
    } else {
        vec![
            requested,
            candidate(normalize_shell_command(env.get("SHELL").map(String::as_str), platform)),
            candidate(Some("/bin/zsh".into())),
            candidate(Some("/bin/bash".into())),
            candidate(Some("/bin/sh".into())),
            candidate(Some("zsh".into())),
            candidate(Some("bash".into())),
            candidate(Some("sh".into())),
        ]
    };
    let mut seen = std::collections::HashSet::new();
    list.into_iter().flatten().filter(|candidate| seen.insert(candidate.format())).collect()
}

/// `isRetryableShellSpawnError`: the shell is missing, so the next candidate is worth a try.
pub fn is_retryable_shell_spawn_error(error: &PtySpawnError) -> bool {
    let message = error.messages().join(" ").to_lowercase();
    message.contains("posix_spawnp failed")
        || message.contains("enoent")
        || message.contains("not found")
        || message.contains("file not found")
        || message.contains("no such file")
}

const TERMINAL_ENV_BLOCKLIST: [&str; 3] = ["PORT", "ELECTRON_RENDERER_PORT", "ELECTRON_RUN_AS_NODE"];

/// `shouldExcludeTerminalEnvKey`: the server's own variables stay out of terminals.
pub fn should_exclude_terminal_env_key(key: &str) -> bool {
    let upper = key.to_uppercase();
    upper.starts_with("T3CODE_") || upper.starts_with("VITE_") || TERMINAL_ENV_BLOCKLIST.contains(&upper.as_str())
}

const APPIMAGE_RUNTIME_ENV_KEYS: [&str; 4] = ["APPIMAGE", "APPDIR", "ARGV0", "OWD"];
const APPIMAGE_PATH_LIKE_ENV_KEYS: [&str; 4] = ["PATH", "LD_LIBRARY_PATH", "XDG_DATA_DIRS", "GSETTINGS_SCHEMA_DIR"];

/// `stripAppImageRuntimeEnv`: an AppImage launch leaks its mount into PATH & co.
pub fn strip_appimage_runtime_env(mut env: BTreeMap<String, String>) -> BTreeMap<String, String> {
    if !env.contains_key("APPIMAGE") && !env.contains_key("APPDIR") {
        return env;
    }
    let app_dir = env.get("APPDIR").map(|dir| dir.trim_end_matches('/').to_owned());
    for key in APPIMAGE_RUNTIME_ENV_KEYS {
        env.remove(key);
    }
    if let Some(app_dir) = app_dir.filter(|dir| !dir.is_empty()) {
        for key in APPIMAGE_PATH_LIKE_ENV_KEYS {
            let Some(value) = env.get(key) else { continue };
            let kept: Vec<&str> = value
                .split(':')
                .filter(|segment| !segment.is_empty() && *segment != app_dir && !segment.starts_with(&format!("{app_dir}/")))
                .collect();
            if kept.is_empty() {
                env.remove(key);
            } else {
                let joined = kept.join(":");
                env.insert(key.to_owned(), joined);
            }
        }
    }
    env
}

/// `createTerminalSpawnEnv`: the server environment minus the blocklist, the session's
/// runtime env on top (`CODEX_HOME` / `CLAUDE_CONFIG_DIR` with `~` expanded),
/// `COLORTERM=truecolor` unless set, and the AppImage scrub.
pub fn create_terminal_spawn_env(base_env: &BTreeMap<String, String>, runtime_env: Option<&BTreeMap<String, String>>) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = base_env
        .iter()
        .filter(|(key, _)| !should_exclude_terminal_env_key(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if let Some(runtime_env) = runtime_env {
        for (key, value) in runtime_env {
            let value = if key == "CODEX_HOME" || key == "CLAUDE_CONFIG_DIR" {
                zc_core::expand_home_path(value).to_string_lossy().into_owned()
            } else {
                value.clone()
            };
            env.insert(key.clone(), value);
        }
    }
    if env.get("COLORTERM").is_none_or(|value| value.is_empty()) {
        env.insert("COLORTERM".into(), "truecolor".into());
    }
    strip_appimage_runtime_env(env)
}

/// `normalizedRuntimeEnv`: `None` for no or empty env (a `BTreeMap` is already sorted).
pub fn normalized_runtime_env(env: Option<&BTreeMap<String, String>>) -> Option<BTreeMap<String, String>> {
    env.filter(|env| !env.is_empty()).cloned()
}

/// The server's environment as strings (`process.env`).
pub fn process_env() -> BTreeMap<String, String> {
    std::env::vars_os()
        .map(|(key, value)| (key.to_string_lossy().into_owned(), value.to_string_lossy().into_owned()))
        .collect()
}

/// `MAX_TERMINAL_LABEL_LENGTH`.
pub const MAX_TERMINAL_LABEL_LENGTH: usize = 128;

/// `truncateTerminalWireLabel`: at most 128 UTF-16 code units (never half a character).
pub fn truncate_terminal_wire_label(value: &str) -> String {
    if js_length(value) <= MAX_TERMINAL_LABEL_LENGTH {
        return value.to_owned();
    }
    let mut units = 0;
    let mut out = String::new();
    for c in value.chars() {
        units += c.len_utf16();
        if units > MAX_TERMINAL_LABEL_LENGTH {
            break;
        }
        out.push(c);
    }
    out
}

/// `getTerminalLabel`: `term-3` / `terminal-3` → `Terminal 3`, anything else as is.
pub fn terminal_label(terminal_id: &str) -> String {
    let lower = terminal_id.to_ascii_lowercase();
    let suffix = lower.strip_prefix("terminal-").or_else(|| lower.strip_prefix("term-"));
    match suffix {
        Some(digits) if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) => {
            format!("Terminal {}", &terminal_id[terminal_id.len() - digits.len()..])
        }
        _ => terminal_id.to_owned(),
    }
}

/// `normalizeChildCommandName`: `[kworker]` / `(sd-pam)` unwrapped, first word, basename,
/// `.exe` dropped on Windows.
pub fn normalize_child_command_name(raw: &str, platform: Platform) -> Option<String> {
    let mut trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if (trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() >= 2) || (trimmed.starts_with('(') && trimmed.ends_with(')') && trimmed.len() >= 2)
    {
        trimmed = trimmed[1..trimmed.len() - 1].trim();
    }
    let first = trimmed.split_whitespace().next().unwrap_or(trimmed).trim();
    if first.is_empty() {
        return None;
    }
    let base = if platform == Platform::Windows {
        first.rsplit(['\\', '/']).next().unwrap_or(first)
    } else {
        first.rsplit('/').next().unwrap_or(first)
    };
    let without_exe = if platform == Platform::Windows && base.to_lowercase().ends_with(".exe") {
        &base[..base.len() - 4]
    } else {
        base
    };
    (!without_exe.is_empty()).then(|| without_exe.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn shell_candidates() {
        let candidates = resolve_shell_candidates("/definitely/missing-shell -l", Platform::Linux, &env(&[("SHELL", "/bin/zsh")]));
        let formatted: Vec<String> = candidates.iter().map(ShellCandidate::format).collect();
        assert_eq!(
            formatted,
            [
                "/definitely/missing-shell",
                "/bin/zsh -o nopromptsp",
                "/bin/bash",
                "/bin/sh",
                "zsh -o nopromptsp",
                "bash",
                "sh"
            ]
        );
        let windows = resolve_shell_candidates(
            "C:\\missing\\custom-shell.exe",
            Platform::Windows,
            &env(&[("ComSpec", "C:\\Windows\\System32\\cmd.exe"), ("SystemRoot", "C:\\Windows")]),
        );
        let shells: Vec<&str> = windows.iter().map(|c| c.shell.as_str()).collect();
        assert_eq!(
            shells,
            [
                "C:\\missing\\custom-shell.exe",
                "pwsh.exe",
                "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe",
                "powershell.exe",
                "C:\\Windows\\System32\\cmd.exe",
                "cmd.exe"
            ]
        );
        assert_eq!(windows[1].args, ["-NoLogo"]);
        assert_eq!(windows[2].args, ["-NoLogo"]);
        assert_eq!(normalize_shell_command(Some("  '/bin/zsh' -l"), Platform::Darwin).as_deref(), Some("/bin/zsh"));
    }

    #[test]
    fn filters_app_runtime_env() {
        let spawn = create_terminal_spawn_env(
            &env(&[
                ("PORT", "5173"),
                ("T3CODE_PORT", "3773"),
                ("t3code_lower", "x"),
                ("VITE_DEV_SERVER_URL", "http://localhost:5173"),
                ("TEST_TERMINAL_KEEP", "keep-me"),
            ]),
            None,
        );
        assert_eq!(spawn, env(&[("COLORTERM", "truecolor"), ("TEST_TERMINAL_KEEP", "keep-me")]));
    }

    #[test]
    fn colorterm_defaults_without_replacing_explicit_values() {
        for (parent, runtime, expected) in [
            (None, None, "truecolor"),
            (Some(""), None, "truecolor"),
            (Some("24bit"), None, "24bit"),
            (Some("24bit"), Some(""), "truecolor"),
            (Some("24bit"), Some("custom"), "custom"),
        ] {
            let base = parent.map_or_else(BTreeMap::new, |p| env(&[("COLORTERM", p)]));
            let runtime = runtime.map(|r| env(&[("COLORTERM", r)]));
            let spawn = create_terminal_spawn_env(&base, runtime.as_ref());
            assert_eq!(spawn["COLORTERM"], expected);
        }
    }

    #[test]
    fn expands_provider_homes_and_injects_overrides() {
        let spawn = create_terminal_spawn_env(
            &env(&[("FORCE_COLOR", "3")]),
            Some(&env(&[
                ("CODEX_HOME", "~/.codex-work"),
                ("CLAUDE_CONFIG_DIR", "~/.claude-work"),
                ("CUSTOM_ACCOUNT", "~/leave-this-value-alone"),
                ("T3CODE_PROJECT_ROOT", "/repo"),
                ("FORCE_COLOR", "0"),
            ])),
        );
        assert!(spawn["CODEX_HOME"].ends_with("/.codex-work"));
        assert!(!spawn["CODEX_HOME"].starts_with('~'));
        assert!(spawn["CLAUDE_CONFIG_DIR"].ends_with("/.claude-work"));
        assert_eq!(spawn["CUSTOM_ACCOUNT"], "~/leave-this-value-alone");
        assert_eq!(spawn["T3CODE_PROJECT_ROOT"], "/repo");
        assert_eq!(spawn["FORCE_COLOR"], "0");
    }

    #[test]
    fn strips_appimage_runtime_env() {
        let app_dir = "/tmp/.mount_T3Codeabc123";
        let spawn = create_terminal_spawn_env(
            &env(&[
                ("APPIMAGE", "/home/user/T3-Code.AppImage"),
                ("APPDIR", app_dir),
                ("ARGV0", "/home/user/T3-Code.AppImage"),
                ("OWD", "/home/user/project"),
                ("PATH", &format!("{app_dir}/usr/bin:{app_dir}:/usr/local/bin:/usr/bin:/bin")),
                ("LD_LIBRARY_PATH", &format!("{app_dir}/usr/lib:/home/user/.local/lib")),
                ("XDG_DATA_DIRS", &format!("{app_dir}/usr/share:/usr/local/share:/usr/share")),
                ("GSETTINGS_SCHEMA_DIR", &format!("{app_dir}/usr/share/glib-2.0/schemas")),
                ("TEST_TERMINAL_KEEP", "keep-me"),
            ]),
            None,
        );
        for key in ["APPIMAGE", "APPDIR", "ARGV0", "OWD", "GSETTINGS_SCHEMA_DIR"] {
            assert!(!spawn.contains_key(key), "{key}");
        }
        assert_eq!(spawn["PATH"], "/usr/local/bin:/usr/bin:/bin");
        assert_eq!(spawn["LD_LIBRARY_PATH"], "/home/user/.local/lib");
        assert_eq!(spawn["XDG_DATA_DIRS"], "/usr/local/share:/usr/share");
        assert_eq!(spawn["TEST_TERMINAL_KEEP"], "keep-me");

        let untouched = create_terminal_spawn_env(&env(&[("PATH", "/usr/bin"), ("OWD", "/home/user/keep-this")]), None);
        assert_eq!(untouched["OWD"], "/home/user/keep-this");
        assert_eq!(untouched["PATH"], "/usr/bin");
    }

    #[test]
    fn labels() {
        assert_eq!(terminal_label("term-1"), "Terminal 1");
        assert_eq!(terminal_label("Terminal-12"), "Terminal 12");
        assert_eq!(terminal_label("term-x"), "term-x");
        assert_eq!(terminal_label("dev-server"), "dev-server");
        assert_eq!(truncate_terminal_wire_label(&"😀".repeat(70)).chars().count(), 64);
        assert_eq!(
            normalize_child_command_name("/usr/bin/python3 -m http.server", Platform::Linux).as_deref(),
            Some("python3")
        );
        assert_eq!(normalize_child_command_name("[kworker/0:1]", Platform::Linux).as_deref(), Some("0:1"));
        assert_eq!(
            normalize_child_command_name("C:\\Windows\\ping.EXE", Platform::Windows).as_deref(),
            Some("ping")
        );
        assert_eq!(normalize_child_command_name("  ", Platform::Linux), None);
    }

    #[test]
    fn retryable_spawn_errors() {
        let retryable = PtySpawnError::new("fake", Some("/x".into()), "posix_spawnp failed.");
        assert!(is_retryable_shell_spawn_error(&retryable));
        let enoent = PtySpawnError::new("fake", Some("x".into()), "spawn custom-shell.exe ENOENT");
        assert!(is_retryable_shell_spawn_error(&enoent));
        let other = PtySpawnError::new("fake", Some("/bin/sh".into()), "permission denied");
        assert!(!is_retryable_shell_spawn_error(&other));
    }
}
