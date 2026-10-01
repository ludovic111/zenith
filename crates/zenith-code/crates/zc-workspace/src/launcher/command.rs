//! Command lookup on `PATH` (`resolveCommandPath`, `isCommandAvailable`, `resolveSpawnCommand`
//! of `packages/shared/src/shell.ts`), for the platform the launcher targets.
//!
//! - A command with a separator is probed as a path; otherwise each `PATH` entry (quotes
//!   stripped, duplicates skipped) is probed in order. On Windows the `PATHEXT` variants are
//!   tried and an executable must carry one of those extensions; elsewhere it must pass
//!   `access(X_OK)`. Only regular files count (symlinks are followed).
//! - Results of `PATH` scans are cached for 30 s per (platform, PATH, PATHEXT, command), 512
//!   entries, oldest evicted first, on the monotonic clock; explicit paths are never cached.
//! - On Windows, `.cmd`/`.bat` shims run through `cmd.exe` with cross-spawn's escaping.
//!
//! Not ported: the per-batch `PATH` directory listings of `withPathDirectoryListings` (a
//! speed-up for Node; plain probes are cheap here, and the results are the same).

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use indexmap::IndexMap;
use tokio::time::Instant;

use crate::paths::join;
use crate::platform::NodePlatform;

const COMMAND_RESOLUTION_CACHE_TTL: Duration = Duration::from_secs(30);
const COMMAND_RESOLUTION_CACHE_MAX_ENTRIES: usize = 512;

/// The environment variables the launcher reads (`BrowserLaunchEnvConfig` and
/// `CommandLookupEnvConfig`): absent variables are absent, empty ones are empty strings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LauncherEnv(pub BTreeMap<String, String>);

/// `BrowserLaunchEnvConfig` keys.
pub const BROWSER_LAUNCH_ENV_KEYS: [&str; 9] = [
    "SYSTEMROOT",
    "windir",
    "WSL_DISTRO_NAME",
    "WSL_INTEROP",
    "SSH_CONNECTION",
    "SSH_TTY",
    "container",
    "DISPLAY",
    "WAYLAND_DISPLAY",
];

/// `CommandLookupEnvConfig` keys.
pub const COMMAND_LOOKUP_ENV_KEYS: [&str; 10] = [
    "PATH",
    "Path",
    "path",
    "PATHEXT",
    "HOME",
    "LOCALAPPDATA",
    "ProgramFiles",
    "ProgramW6432",
    "XDG_DATA_HOME",
    "ProgramFiles(x86)",
];

impl LauncherEnv {
    /// The given keys from the process environment.
    pub fn from_process(keys: &[&str]) -> Self {
        Self(
            keys.iter()
                .filter_map(|key| std::env::var(key).ok().map(|value| ((*key).to_owned(), value)))
                .collect(),
        )
    }

    /// Both key sets from the process environment.
    pub fn all_from_process() -> Self {
        let keys: Vec<&str> = BROWSER_LAUNCH_ENV_KEYS.iter().chain(COMMAND_LOOKUP_ENV_KEYS.iter()).copied().collect();
        Self::from_process(&keys)
    }

    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        Self(pairs.into_iter().map(|(key, value)| (key.to_owned(), value.to_owned())).collect())
    }

    /// The value when the variable is defined (`env.X !== undefined`).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    /// The value when it is a non-empty string (`env.X ? … : …`).
    pub fn truthy(&self, key: &str) -> Option<&str> {
        self.get(key).filter(|value| !value.is_empty())
    }

    /// Only the given keys.
    pub fn only(&self, keys: &[&str]) -> Self {
        Self(
            self.0
                .iter()
                .filter(|(key, _)| keys.contains(&key.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        )
    }

    /// `env.PATH ?? env.Path ?? env.path`.
    fn path(&self) -> Option<&str> {
        self.get("PATH").or_else(|| self.get("Path")).or_else(|| self.get("path"))
    }
}

/// `resolveWindowsPathExtensions`.
pub fn windows_path_extensions(env: &LauncherEnv) -> Vec<String> {
    let fallback = || [".COM", ".EXE", ".BAT", ".CMD"].map(str::to_owned).to_vec();
    let Some(raw) = env.truthy("PATHEXT") else { return fallback() };
    let mut parsed: Vec<String> = Vec::new();
    for entry in raw.split(';') {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        let extension = if trimmed.starts_with('.') {
            trimmed.to_uppercase()
        } else {
            format!(".{}", trimmed.to_uppercase())
        };
        if !parsed.contains(&extension) {
            parsed.push(extension);
        }
    }
    if parsed.is_empty() {
        fallback()
    } else {
        parsed
    }
}

/// `path.extname` (POSIX: the last `.` of the base name, not a leading one).
pub fn extname(path: &str) -> &str {
    let base_start = path.rfind('/').map_or(0, |index| index + 1);
    let base = &path[base_start..];
    match base.rfind('.') {
        Some(0) | None => "",
        Some(index) => &base[index..],
    }
}

/// `path.win32.extname`: separators are `/` and `\`.
fn win32_extname(path: &str) -> &str {
    let base_start = path.rfind(['/', '\\']).map_or(0, |index| index + 1);
    let base = &path[base_start..];
    match base.rfind('.') {
        Some(0) | None => "",
        Some(index) => &base[index..],
    }
}

/// `resolveCommandCandidates`.
fn command_candidates(command: &str, platform: NodePlatform, extensions: &[String]) -> Vec<String> {
    if platform != NodePlatform::Win32 {
        return vec![command.to_owned()];
    }
    let extension = extname(command);
    let upper = extension.to_uppercase();
    let mut candidates: Vec<String> = Vec::new();
    let mut push = |candidate: String| {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    };
    if !extension.is_empty() && extensions.contains(&upper) {
        let stem = &command[..command.len() - extension.len()];
        push(command.to_owned());
        push(format!("{stem}{upper}"));
        push(format!("{stem}{}", upper.to_lowercase()));
        return candidates;
    }
    for extension in extensions {
        push(format!("{command}{extension}"));
        push(format!("{command}{}", extension.to_lowercase()));
    }
    candidates
}

fn can_execute(path: &str) -> bool {
    let Ok(c_path) = std::ffi::CString::new(path) else { return false };
    // SAFETY: a valid NUL-terminated path.
    unsafe { libc::access(c_path.as_ptr(), libc::X_OK) == 0 }
}

/// `isExecutableFile`.
fn is_executable_file(path: &str, platform: NodePlatform, extensions: &[String]) -> bool {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        _ => return false,
    }
    if platform == NodePlatform::Win32 {
        let extension = extname(path);
        return !extension.is_empty() && extensions.contains(&extension.to_uppercase());
    }
    can_execute(path)
}

fn strip_wrapping_quotes(value: &str) -> &str {
    value.trim_matches('"')
}

struct CacheEntry {
    resolved: Option<String>,
    expires_at: Instant,
}

/// The command lookup with its process-wide 30 s cache (`CommandResolutionCache`).
#[derive(Default)]
pub struct CommandResolver {
    cache: Mutex<IndexMap<String, CacheEntry>>,
}

impl std::fmt::Debug for CommandResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandResolver").finish_non_exhaustive()
    }
}

impl CommandResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// `resolveCommandPath(command, { env })`: the executable's path, or `None`.
    pub fn resolve_command_path(&self, command: &str, env: &LauncherEnv, platform: NodePlatform) -> Option<String> {
        let extensions = if platform == NodePlatform::Win32 {
            windows_path_extensions(env)
        } else {
            Vec::new()
        };
        let candidates = command_candidates(command, platform, &extensions);
        if command.contains('/') || command.contains('\\') {
            return candidates.into_iter().find(|candidate| is_executable_file(candidate, platform, &extensions));
        }
        let path_value = env.path().unwrap_or("");
        if path_value.is_empty() {
            return None;
        }
        let key = [platform.as_str(), path_value, &extensions.join(";"), command].join("\0");
        let now = Instant::now();
        if let Some(entry) = self.cache.lock().unwrap().get(&key) {
            if entry.expires_at > now {
                return entry.resolved.clone();
            }
        }
        let delimiter = if platform == NodePlatform::Win32 { ';' } else { ':' };
        let mut seen: Vec<&str> = Vec::new();
        let mut resolved = None;
        'entries: for entry in path_value.split(delimiter) {
            let entry = strip_wrapping_quotes(entry.trim());
            if entry.is_empty() || seen.contains(&entry) {
                continue;
            }
            seen.push(entry);
            for candidate in &candidates {
                let candidate_path = join(entry, candidate);
                if is_executable_file(&candidate_path, platform, &extensions) {
                    resolved = Some(candidate_path);
                    break 'entries;
                }
            }
        }
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= COMMAND_RESOLUTION_CACHE_MAX_ENTRIES && !cache.contains_key(&key) {
            cache.shift_remove_index(0);
        }
        cache.insert(
            key,
            CacheEntry {
                resolved: resolved.clone(),
                expires_at: now + COMMAND_RESOLUTION_CACHE_TTL,
            },
        );
        resolved
    }

    /// `isCommandAvailable(command, { env })`.
    pub fn is_command_available(&self, command: &str, env: &LauncherEnv, platform: NodePlatform) -> bool {
        self.resolve_command_path(command, env, platform).is_some()
    }
}

/// `SpawnExecutableResolution`: on Windows, the executable a spawn will run (to detect `.cmd`
/// shims). Injectable like the TS reference.
pub type SpawnExecutableResolver = std::sync::Arc<dyn Fn(&str, NodePlatform, &LauncherEnv) -> Option<String> + Send + Sync>;

/// `resolveSpawnExecutableWithNode`, with Windows path joining.
pub fn default_spawn_executable_resolver() -> SpawnExecutableResolver {
    std::sync::Arc::new(|command: &str, platform: NodePlatform, env: &LauncherEnv| {
        let extensions = if platform == NodePlatform::Win32 {
            windows_path_extensions(env)
        } else {
            Vec::new()
        };
        let candidates = command_candidates(command, platform, &extensions);
        let is_executable = |candidate: &str| -> bool {
            match std::fs::metadata(candidate) {
                Ok(metadata) if metadata.is_file() => {}
                _ => return false,
            }
            if platform == NodePlatform::Win32 {
                return extensions.contains(&win32_extname(candidate).to_uppercase());
            }
            can_execute(candidate)
        };
        if command.contains('/') || command.contains('\\') {
            return candidates.into_iter().find(|candidate| is_executable(candidate));
        }
        let delimiter = if platform == NodePlatform::Win32 { ';' } else { ':' };
        let separator = if platform == NodePlatform::Win32 { "\\" } else { "/" };
        for entry in env.path().unwrap_or("").split(delimiter) {
            let entry = strip_wrapping_quotes(entry.trim());
            if entry.is_empty() {
                continue;
            }
            for candidate in &candidates {
                let path = format!("{}{separator}{candidate}", entry.trim_end_matches(['/', '\\']));
                if is_executable(&path) {
                    return Some(path);
                }
            }
        }
        None
    })
}

/// A command ready to spawn (`ResolvedSpawnCommand`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnCommand {
    pub command: String,
    pub args: Vec<String>,
    /// Run through the platform shell (`cmd.exe` for Windows shims).
    pub shell: bool,
}

/// `escapeWindowsShellArg` (cross-spawn's escaping for `cmd.exe`).
pub fn escape_windows_shell_arg(arg: &str) -> String {
    // Double the backslashes that precede a quote, then escape the quote.
    let mut escaped = String::with_capacity(arg.len() + 8);
    let mut backslashes = 0usize;
    for character in arg.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                escaped.push_str(&"\\".repeat(backslashes * 2));
                escaped.push_str("\\\"");
                backslashes = 0;
                continue;
            }
            _ => {
                escaped.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                escaped.push(character);
                continue;
            }
        }
    }
    // Trailing backslashes are doubled so the closing quote survives.
    escaped.push_str(&"\\".repeat(backslashes * 2));
    let quoted = format!("\"{escaped}\"");
    let mut out = String::with_capacity(quoted.len() * 2);
    for character in quoted.chars() {
        if "()][%!^\"`<>&|;, *?".contains(character) {
            out.push('^');
        }
        out.push(character);
    }
    out
}

/// `resolveSpawnCommand(command, args, { env })`.
pub fn resolve_spawn_command(command: &str, args: &[String], env: &LauncherEnv, platform: NodePlatform, resolver: &SpawnExecutableResolver) -> SpawnCommand {
    if platform != NodePlatform::Win32 {
        return SpawnCommand {
            command: command.to_owned(),
            args: args.to_vec(),
            shell: false,
        };
    }
    let resolved = resolver(command, platform, env).unwrap_or_else(|| command.to_owned());
    let extension = win32_extname(&resolved).to_lowercase();
    if extension != ".cmd" && extension != ".bat" {
        return SpawnCommand {
            command: resolved,
            args: args.to_vec(),
            shell: false,
        };
    }
    SpawnCommand {
        command: escape_windows_shell_arg(&resolved),
        args: args.iter().map(|arg| escape_windows_shell_arg(arg)).collect(),
        shell: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn escapes_cmd_arguments_like_cross_spawn() {
        assert_eq!(
            escape_windows_shell_arg("C:\\Program Files\\Microsoft VS Code\\bin\\code.CMD"),
            "^\"C:\\Program^ Files\\Microsoft^ VS^ Code\\bin\\code.CMD^\""
        );
        assert_eq!(escape_windows_shell_arg("--goto"), "^\"--goto^\"");
        assert_eq!(escape_windows_shell_arg("a\\\"b"), "^\"a\\\\\\^\"b^\"");
        assert_eq!(escape_windows_shell_arg("dir\\"), "^\"dir\\\\^\"");
        assert_eq!(escape_windows_shell_arg("x&y|z"), "^\"x^&y^|z^\"");
    }

    #[test]
    fn extensions_and_candidates() {
        let env = LauncherEnv::from_pairs([("PATHEXT", ".com;exe; .Bat ;;.CMD;.exe")]);
        assert_eq!(windows_path_extensions(&env), [".COM", ".EXE", ".BAT", ".CMD"]);
        assert_eq!(windows_path_extensions(&LauncherEnv::default()), [".COM", ".EXE", ".BAT", ".CMD"]);
        let extensions = windows_path_extensions(&LauncherEnv::default());
        assert_eq!(command_candidates("code.cmd", NodePlatform::Win32, &extensions), ["code.cmd", "code.CMD"]);
        assert_eq!(command_candidates("code", NodePlatform::Darwin, &extensions), ["code"]);
        assert_eq!(extname("/a/b.c/d"), "");
        assert_eq!(extname("/a/.bashrc"), "");
        assert_eq!(extname("/a/x.tar.gz"), ".gz");
    }

    #[test]
    fn finds_executables_on_path_and_caches_scans() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let tool = bin.join("tool");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        let bin_path = bin.to_string_lossy().into_owned();
        let env = LauncherEnv::from_pairs([("PATH", format!("\"{bin_path}\":/nonexistent").as_str())]);
        let resolver = CommandResolver::new();
        // Not executable yet.
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(resolver.resolve_command_path("tool", &env, NodePlatform::Linux), None);
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        // The negative result is cached for 30 s.
        assert_eq!(resolver.resolve_command_path("tool", &env, NodePlatform::Linux), None);
        // Explicit paths are never cached.
        let tool_path = tool.to_string_lossy().into_owned();
        assert_eq!(resolver.resolve_command_path(&tool_path, &env, NodePlatform::Linux), Some(tool_path.clone()));
        assert_eq!(CommandResolver::new().resolve_command_path("tool", &env, NodePlatform::Linux), Some(tool_path));
        // A directory is not a command.
        std::fs::create_dir(bin.join("folder")).unwrap();
        assert!(!CommandResolver::new().is_command_available("folder", &env, NodePlatform::Linux));
    }
}
