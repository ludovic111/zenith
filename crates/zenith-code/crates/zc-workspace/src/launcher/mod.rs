//! `ExternalLauncher` (`apps/server/src/process/externalLauncher.ts`): which editors are
//! installed (`server.getConfig` → `availableEditors`), whether the file manager can reveal a
//! file (`fileManagerRevealKind`), and `shell.openInEditor` / browser launches.
//!
//! - Discovery walks the editor catalogue ([`editors::EDITORS`]): an editor is available when
//!   one of its commands is on `PATH` or an executable sits in a known install location. The
//!   file manager is available when the platform's command is usable (`open`, `explorer`, the
//!   WSL Explorer bridge, or `xdg-open` with a graphical session *and* an `inode/directory`
//!   handler, probed with `xdg-mime` under its own 2 s timeout). The discovered list is
//!   memoized for 60 s, and only once a scan completes (a cancelled scan caches nothing).
//! - Launch styles: `goto` adds `--goto` when the target carries `:line[:column]`,
//!   `line-column` turns it into `--line L [--column C] path`, `direct-path` passes it as is.
//! - Launches are detached, with stdio ignored; on Windows `.cmd` shims go through `cmd.exe`
//!   with cross-spawn escaping, and Explorer reveals go through PowerShell with an encoded
//!   command so the raw `/select,"<path>"` switch survives.

pub mod command;
pub mod editors;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use futures::future::BoxFuture;
use futures::FutureExt;
use regex::Regex;
use serde_json::json;
use tokio::time::Instant;
use zc_contracts::{
    EditorId, EditorLaunchStyle, FileManagerRevealKind, LaunchEditorInput, LitExternalLauncherBrowserSpawnError, LitExternalLauncherCommandNotFoundError,
    LitExternalLauncherEditorSpawnError, LitExternalLauncherUnknownEditorError, LitExternalLauncherUnsupportedEditorError,
};
pub use zc_contracts::{
    ExternalLauncherBrowserSpawnError, ExternalLauncherCommandNotFoundError, ExternalLauncherEditorSpawnError, ExternalLauncherError,
    ExternalLauncherUnknownEditorError, ExternalLauncherUnsupportedEditorError,
};

use self::command::{
    default_spawn_executable_resolver, resolve_spawn_command, CommandResolver, LauncherEnv, SpawnExecutableResolver, BROWSER_LAUNCH_ENV_KEYS,
    COMMAND_LOOKUP_ENV_KEYS,
};
use self::editors::{find_editor, resolve_editor_command, EditorDefinition, EDITORS};
use crate::paths::dirname;
use crate::platform::NodePlatform;

const POWERSHELL_ARGUMENTS_PREFIX: [&str; 5] = ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand"];
const WSL_POWERSHELL_COMMAND: &str = "powershell.exe";
const LINUX_DIRECTORY_HANDLER_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const EDITOR_DISCOVERY_CACHE_TTL: Duration = Duration::from_secs(60);

/// A process to start detached, stdio ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachedLaunch {
    pub command: String,
    pub args: Vec<String>,
    /// Through the platform shell (`shell: true`).
    pub shell: bool,
}

/// How the launcher starts processes (the `ChildProcessSpawner` the TS tests replace).
pub trait LaunchSpawner: Send + Sync + 'static {
    /// Starts a detached process and lets it go (`spawn` + `unref`).
    fn spawn_detached(&self, launch: &DetachedLaunch) -> std::io::Result<()>;
    /// Runs a short probe (stdin and stderr ignored) to completion: `(exitCode, stdout)`.
    /// `None` when it cannot run.
    fn run_probe(&self, command: &str, args: &[String]) -> BoxFuture<'static, Option<(i32, String)>>;
}

/// [`LaunchSpawner`] on real processes.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemSpawner;

impl LaunchSpawner for SystemSpawner {
    fn spawn_detached(&self, launch: &DetachedLaunch) -> std::io::Result<()> {
        let mut command = if launch.shell {
            // `spawn(command, args, { shell: true })`: Node joins with spaces.
            let line = std::iter::once(launch.command.as_str())
                .chain(launch.args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            if cfg!(windows) {
                let mut command = tokio::process::Command::new("cmd.exe");
                command.args(["/d", "/s", "/c", &format!("\"{line}\"")]);
                command
            } else {
                let mut command = tokio::process::Command::new("/bin/sh");
                command.args(["-c", &line]);
                command
            }
        } else {
            let mut command = tokio::process::Command::new(&launch.command);
            command.args(&launch.args);
            command
        };
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(false);
        #[cfg(unix)]
        {
            // `detached: true`: a new session, so the editor outlives the server.
            command.process_group(0);
        }
        // Dropping the child detaches it; tokio reaps it in the background.
        command.spawn().map(drop)
    }

    fn run_probe(&self, command: &str, args: &[String]) -> BoxFuture<'static, Option<(i32, String)>> {
        let mut process = tokio::process::Command::new(command);
        process
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true);
        async move {
            let output = process.output().await.ok()?;
            Some((output.status.code().unwrap_or(-1), String::from_utf8_lossy(&output.stdout).into_owned()))
        }
        .boxed()
    }
}

/// Where the launcher reads its environment.
#[derive(Debug, Clone)]
pub enum EnvSource {
    /// The process environment, read at every call (like Effect `Config`).
    Process,
    /// A fixed environment (tests, embedders).
    Fixed(LauncherEnv),
}

/// A resolved editor launch (`EditorLaunch`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorLaunch {
    pub editor: EditorId,
    pub target: String,
    pub command: String,
    pub args: Vec<String>,
}

struct DiscoveryCache {
    editors: Vec<EditorId>,
    expires_at: Instant,
}

/// The `ExternalLauncher` service.
pub struct ExternalLauncher {
    platform: NodePlatform,
    env: EnvSource,
    spawner: Arc<dyn LaunchSpawner>,
    resolver: CommandResolver,
    spawn_resolver: SpawnExecutableResolver,
    discovery: Mutex<Option<DiscoveryCache>>,
}

impl std::fmt::Debug for ExternalLauncher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalLauncher").field("platform", &self.platform).finish_non_exhaustive()
    }
}

impl Default for ExternalLauncher {
    fn default() -> Self {
        Self::new(NodePlatform::current(), EnvSource::Process, Arc::new(SystemSpawner))
    }
}

fn target_position_pattern() -> &'static Regex {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"^(.*?):(\d+)(?::(\d+))?$").expect("valid target pattern"))
}

/// `parseTargetPathAndPosition`: `(path, line, column)`.
fn parse_target_path_and_position(target: &str) -> Option<(String, String, Option<String>)> {
    let captures = target_position_pattern().captures(target)?;
    let path = captures.get(1).map(|m| m.as_str()).filter(|path| !path.is_empty())?;
    let line = captures.get(2).map(|m| m.as_str()).filter(|line| !line.is_empty())?;
    Some((path.to_owned(), line.to_owned(), captures.get(3).map(|m| m.as_str().to_owned())))
}

/// `resolveCommandEditorArgs`.
pub fn resolve_command_editor_args(editor: &EditorDefinition, target: &str) -> Vec<String> {
    let parsed = parse_target_path_and_position(target);
    match editor.launch_style {
        EditorLaunchStyle::DirectPath => vec![target.to_owned()],
        EditorLaunchStyle::Goto => match parsed {
            Some(_) => vec!["--goto".into(), target.to_owned()],
            None => vec![target.to_owned()],
        },
        EditorLaunchStyle::LineColumn => match parsed {
            None => vec![target.to_owned()],
            Some((path, line, column)) => {
                let mut args = vec!["--line".to_owned(), line];
                if let Some(column) = column {
                    args.push("--column".into());
                    args.push(column);
                }
                args.push(path);
                args
            }
        },
    }
}

/// `encodeUtf16LeBase64`: what `-EncodedCommand` expects.
pub fn encode_utf16le_base64(input: &str) -> String {
    let bytes: Vec<u8> = input.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn escape_powershell_string_literal(input: &str) -> String {
    format!("'{}'", input.replace('\'', "''"))
}

fn resolve_powershell_path(env: &LauncherEnv) -> String {
    let root = env.truthy("SYSTEMROOT").or_else(|| env.truthy("windir")).unwrap_or("C:\\Windows");
    format!("{root}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe")
}

const WSL_POWERSHELL_PATH: &str = "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe";

fn should_use_windows_host_from_wsl(platform: NodePlatform, env: &LauncherEnv) -> bool {
    platform == NodePlatform::Linux
        && (env.get("WSL_DISTRO_NAME").is_some() || env.get("WSL_INTEROP").is_some())
        && env.get("SSH_CONNECTION").is_none()
        && env.get("SSH_TTY").is_none()
        && env.get("container").is_none()
}

fn has_graphical_linux_session(env: &LauncherEnv) -> bool {
    ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|key| env.get(key).is_some_and(|value| !value.trim().is_empty()))
}

fn wsl_distro_name(env: &LauncherEnv) -> Option<&str> {
    env.get("WSL_DISTRO_NAME").filter(|name| !name.trim().is_empty())
}

fn file_manager_command_for_platform(platform: NodePlatform, env: &LauncherEnv) -> Option<&'static str> {
    match platform {
        NodePlatform::Darwin => Some("open"),
        NodePlatform::Win32 => Some("explorer"),
        _ if should_use_windows_host_from_wsl(platform, env) => wsl_distro_name(env).map(|_| "explorer.exe"),
        _ => has_graphical_linux_session(env).then_some("xdg-open"),
    }
}

fn resolve_wsl_file_manager_path(target: &str, distro_name: &str) -> String {
    let relative = target.trim_start_matches('/').replace('/', "\\");
    if relative.is_empty() {
        format!("\\\\wsl.localhost\\{distro_name}")
    } else {
        format!("\\\\wsl.localhost\\{distro_name}\\{relative}")
    }
}

/// `buildFileExplorerRevealPowerShellSource`: Explorer's `/select,"<path>"` with only the path
/// quoted, through `Start-Process -ArgumentList` so it reaches Explorer verbatim.
pub fn build_file_explorer_reveal_powershell_source(explorer_command: &str, target: &str) -> String {
    format!(
        "$ProgressPreference = 'SilentlyContinue'; Start-Process {} -ArgumentList ('/select,\"' + {} + '\"')",
        escape_powershell_string_literal(explorer_command),
        escape_powershell_string_literal(target)
    )
}

fn file_explorer_reveal_launch(target: &str, explorer_target: &str, powershell_command: &str) -> EditorLaunch {
    let mut args: Vec<String> = POWERSHELL_ARGUMENTS_PREFIX.iter().map(|arg| (*arg).to_owned()).collect();
    args.push(encode_utf16le_base64(&build_file_explorer_reveal_powershell_source(
        "explorer.exe",
        explorer_target,
    )));
    EditorLaunch {
        editor: EditorId::FileManager,
        target: target.to_owned(),
        command: powershell_command.to_owned(),
        args,
    }
}

fn windows_browser_launch(target: &str, command: &str) -> DetachedLaunch {
    let source = format!("$ProgressPreference = 'SilentlyContinue'; Start {}", escape_powershell_string_literal(target));
    let mut args: Vec<String> = POWERSHELL_ARGUMENTS_PREFIX.iter().map(|arg| (*arg).to_owned()).collect();
    args.push(encode_utf16le_base64(&source));
    DetachedLaunch {
        command: command.to_owned(),
        args,
        shell: false,
    }
}

/// A spawn failure as a defect, shaped like Node's (`spawn <command> ENOENT`).
fn spawn_defect(command: &str, error: &std::io::Error) -> serde_json::Value {
    let (code, _) = crate::errors::node_error_code(error);
    let message = if code == "UNKNOWN" {
        format!("spawn {command}: {error}")
    } else {
        format!("spawn {command} {code}")
    };
    json!({ "name": "Error", "message": message })
}

impl ExternalLauncher {
    pub fn new(platform: NodePlatform, env: EnvSource, spawner: Arc<dyn LaunchSpawner>) -> Self {
        Self {
            platform,
            env,
            spawner,
            resolver: CommandResolver::new(),
            spawn_resolver: default_spawn_executable_resolver(),
            discovery: Mutex::new(None),
        }
    }

    /// Replaces the Windows spawn-executable resolution (`SpawnExecutableResolution`).
    pub fn with_spawn_resolver(mut self, resolver: SpawnExecutableResolver) -> Self {
        self.spawn_resolver = resolver;
        self
    }

    pub fn platform(&self) -> NodePlatform {
        self.platform
    }

    fn read_env(&self, keys: &[&str]) -> LauncherEnv {
        match &self.env {
            EnvSource::Process => LauncherEnv::from_process(keys),
            EnvSource::Fixed(env) => env.only(keys),
        }
    }

    /// `{ ...browserLaunchEnv, ...commandLookupEnv }`.
    fn full_env(&self) -> LauncherEnv {
        let keys: Vec<&str> = BROWSER_LAUNCH_ENV_KEYS.iter().chain(COMMAND_LOOKUP_ENV_KEYS.iter()).copied().collect();
        self.read_env(&keys)
    }

    fn available(&self, command: &str, env: &LauncherEnv) -> bool {
        self.resolver.is_command_available(command, env, self.platform)
    }

    /// `hasUsableLinuxDirectoryHandler`.
    async fn has_usable_linux_directory_handler(&self, env: &LauncherEnv) -> bool {
        if !self.available("xdg-mime", env) {
            return false;
        }
        let args = ["query", "default", "inode/directory"].map(str::to_owned);
        let probe = self.spawner.run_probe("xdg-mime", &args);
        match tokio::time::timeout(LINUX_DIRECTORY_HANDLER_PROBE_TIMEOUT, probe).await {
            Ok(Some((exit_code, stdout))) => exit_code == 0 && !stdout.trim().is_empty(),
            _ => false,
        }
    }

    /// `isUsableFileManagerCommand`.
    async fn is_usable_file_manager_command(&self, command: &str, env: &LauncherEnv) -> bool {
        if !self.available(command, env) {
            return false;
        }
        command != "xdg-open" || self.has_usable_linux_directory_handler(env).await
    }

    /// `resolveUsableFileManagerCommand`.
    async fn resolve_usable_file_manager_command(&self, env: &LauncherEnv) -> Option<&'static str> {
        if let Some(command) = file_manager_command_for_platform(self.platform, env) {
            if self.is_usable_file_manager_command(command, env).await {
                return Some(command);
            }
        }
        if should_use_windows_host_from_wsl(self.platform, env)
            && has_graphical_linux_session(env)
            && self.is_usable_file_manager_command("xdg-open", env).await
        {
            return Some("xdg-open");
        }
        None
    }

    async fn scan_available_editors(&self) -> Vec<EditorId> {
        let env = self.full_env();
        let mut available = Vec::new();
        for editor in EDITORS {
            if editor.commands.is_none() {
                if self.resolve_usable_file_manager_command(&env).await.is_some() {
                    available.push(editor.id);
                }
                continue;
            }
            if resolve_editor_command(editor, &env, self.platform, &self.resolver).is_some() {
                available.push(editor.id);
            }
        }
        available
    }

    /// `resolveAvailableEditors()`, memoized for 60 s once a scan completes.
    pub async fn resolve_available_editors(&self) -> Vec<EditorId> {
        let now = Instant::now();
        if let Some(cache) = self.discovery.lock().unwrap().as_ref() {
            if cache.expires_at > now {
                return cache.editors.clone();
            }
        }
        let editors = self.scan_available_editors().await;
        *self.discovery.lock().unwrap() = Some(DiscoveryCache {
            editors: editors.clone(),
            expires_at: now + EDITOR_DISCOVERY_CACHE_TTL,
        });
        editors
    }

    /// `resolveFileManagerRevealKind()`: only meaningful when the file manager is available.
    pub async fn resolve_file_manager_reveal_kind(&self) -> Option<FileManagerRevealKind> {
        let env = self.full_env();
        match self.platform {
            NodePlatform::Darwin => Some(FileManagerRevealKind::Finder),
            NodePlatform::Win32 => self
                .available(&resolve_powershell_path(&env), &env)
                .then_some(FileManagerRevealKind::FileExplorer),
            platform if should_use_windows_host_from_wsl(platform, &env) => {
                if wsl_distro_name(&env).is_some() && self.available("explorer.exe", &env) && self.available(WSL_POWERSHELL_COMMAND, &env) {
                    return Some(FileManagerRevealKind::FileExplorer);
                }
                (has_graphical_linux_session(&env) && self.is_usable_file_manager_command("xdg-open", &env).await).then_some(FileManagerRevealKind::Files)
            }
            _ => has_graphical_linux_session(&env).then_some(FileManagerRevealKind::Files),
        }
    }

    /// `resolveFileManagerRevealLaunch`.
    async fn resolve_file_manager_reveal_launch(&self, target: &str, env: &LauncherEnv, command: &str) -> EditorLaunch {
        let launch = |command: &str, args: Vec<String>| EditorLaunch {
            editor: EditorId::FileManager,
            target: target.to_owned(),
            command: command.to_owned(),
            args,
        };
        match self.platform {
            NodePlatform::Darwin => return launch("open", vec!["-R".into(), target.to_owned()]),
            NodePlatform::Win32 => return file_explorer_reveal_launch(target, &target.replace('/', "\\"), &resolve_powershell_path(env)),
            _ => {}
        }
        if command == "explorer.exe" && should_use_windows_host_from_wsl(self.platform, env) {
            if let Some(distro) = env.get("WSL_DISTRO_NAME") {
                let explorer_target = resolve_wsl_file_manager_path(target, distro);
                if self.available(WSL_POWERSHELL_COMMAND, env) {
                    // Explorer's raw switch cannot express a double quote: open the parent.
                    if explorer_target.contains('"') {
                        return launch("explorer.exe", vec![resolve_wsl_file_manager_path(&dirname(target), distro)]);
                    }
                    return file_explorer_reveal_launch(target, &explorer_target, WSL_POWERSHELL_COMMAND);
                }
                if has_graphical_linux_session(env) && self.is_usable_file_manager_command("xdg-open", env).await {
                    return launch("xdg-open", vec![dirname(target)]);
                }
                return launch("explorer.exe", vec![resolve_wsl_file_manager_path(&dirname(target), distro)]);
            }
        }
        // Linux file managers have no portable "select" flag: open the containing directory.
        launch(command, vec![dirname(target)])
    }

    /// `resolveEditorLaunch(input)`, by editor name (an unknown name is
    /// `ExternalLauncherUnknownEditorError`).
    pub async fn resolve_editor_launch(&self, editor: &str, cwd: &str, reveal: bool) -> Result<EditorLaunch, ExternalLauncherError> {
        let env = self.full_env();
        let Some(definition) = find_editor(editor) else {
            return Err(ExternalLauncherError::ExternalLauncherUnknownEditorError(ExternalLauncherUnknownEditorError {
                tag: LitExternalLauncherUnknownEditorError,
                editor: editor.to_owned(),
            }));
        };
        if let Some(commands) = definition.commands {
            let resolved = resolve_editor_command(definition, &env, self.platform, &self.resolver);
            let (command, base_args) = match resolved {
                Some(resolved) => (resolved.command, resolved.base_args),
                None => (commands[0].to_owned(), definition.base_args.iter().map(|arg| (*arg).to_owned()).collect()),
            };
            let mut args = base_args;
            args.extend(resolve_command_editor_args(definition, cwd));
            return Ok(EditorLaunch {
                editor: definition.id,
                target: cwd.to_owned(),
                command,
                args,
            });
        }
        let unsupported = || {
            ExternalLauncherError::ExternalLauncherUnsupportedEditorError(ExternalLauncherUnsupportedEditorError {
                tag: LitExternalLauncherUnsupportedEditorError,
                editor: definition.id,
            })
        };
        if definition.id != EditorId::FileManager {
            return Err(unsupported());
        }
        let Some(command) = self.resolve_usable_file_manager_command(&env).await else {
            return Err(unsupported());
        };
        if reveal {
            return Ok(self.resolve_file_manager_reveal_launch(cwd, &env, command).await);
        }
        let args = match env.get("WSL_DISTRO_NAME") {
            Some(distro) if command == "explorer.exe" => vec![resolve_wsl_file_manager_path(cwd, distro)],
            _ => vec![cwd.to_owned()],
        };
        Ok(EditorLaunch {
            editor: definition.id,
            target: cwd.to_owned(),
            command: command.to_owned(),
            args,
        })
    }

    /// `launchEditorProcess(launch)`.
    fn launch_editor_process(&self, launch: &EditorLaunch) -> Result<(), ExternalLauncherError> {
        let env = self.read_env(&COMMAND_LOOKUP_ENV_KEYS);
        if !self.available(&launch.command, &env) {
            return Err(ExternalLauncherError::ExternalLauncherCommandNotFoundError(
                ExternalLauncherCommandNotFoundError {
                    tag: LitExternalLauncherCommandNotFoundError,
                    editor: launch.editor,
                    command: launch.command.clone(),
                },
            ));
        }
        let spawn = resolve_spawn_command(&launch.command, &launch.args, &env, self.platform, &self.spawn_resolver);
        self.spawner
            .spawn_detached(&DetachedLaunch {
                command: spawn.command.clone(),
                args: spawn.args.clone(),
                shell: spawn.shell,
            })
            .map_err(|error| {
                ExternalLauncherError::ExternalLauncherEditorSpawnError(ExternalLauncherEditorSpawnError {
                    tag: LitExternalLauncherEditorSpawnError,
                    cause: spawn_defect(&spawn.command, &error),
                    command: spawn.command.clone(),
                    args: spawn.args.clone(),
                    editor: launch.editor,
                    target: launch.target.clone(),
                })
            })
    }

    /// `launchEditor(input)` by editor name.
    pub async fn launch_editor_by_name(&self, editor: &str, cwd: &str, reveal: bool) -> Result<(), ExternalLauncherError> {
        let launch = self.resolve_editor_launch(editor, cwd, reveal).await?;
        self.launch_editor_process(&launch)
    }

    /// `launchEditor(input)` (`shell.openInEditor`).
    pub async fn launch_editor(&self, input: &LaunchEditorInput) -> Result<(), ExternalLauncherError> {
        self.launch_editor_by_name(input.editor.as_str(), &input.cwd, input.reveal == Some(true)).await
    }

    /// `buildBrowserLaunch(target)`.
    pub fn browser_launch(&self, target: &str) -> DetachedLaunch {
        let env = self.read_env(&BROWSER_LAUNCH_ENV_KEYS);
        match self.platform {
            NodePlatform::Darwin => DetachedLaunch {
                command: "open".into(),
                args: vec![target.to_owned()],
                shell: false,
            },
            NodePlatform::Win32 => windows_browser_launch(target, &resolve_powershell_path(&env)),
            platform if should_use_windows_host_from_wsl(platform, &env) => windows_browser_launch(target, WSL_POWERSHELL_PATH),
            _ => DetachedLaunch {
                command: "xdg-open".into(),
                args: vec![target.to_owned()],
                shell: false,
            },
        }
    }

    /// `launchBrowser(target)`.
    pub fn launch_browser(&self, target: &str) -> Result<(), ExternalLauncherError> {
        let launch = self.browser_launch(target);
        self.spawner.spawn_detached(&launch).map_err(|error| {
            ExternalLauncherError::ExternalLauncherBrowserSpawnError(ExternalLauncherBrowserSpawnError {
                tag: LitExternalLauncherBrowserSpawnError,
                cause: spawn_defect(&launch.command, &error),
                command: launch.command.clone(),
                args: launch.args.clone(),
                target: target.to_owned(),
            })
        })
    }
}

/// `ExternalLauncherError.message` (the TS getters), for logs.
pub fn external_launcher_error_message(error: &ExternalLauncherError) -> String {
    match error {
        ExternalLauncherError::ExternalLauncherUnknownEditorError(error) => format!("Unknown editor: {}", error.editor),
        ExternalLauncherError::ExternalLauncherUnsupportedEditorError(error) => {
            format!("Unsupported editor: {}", error.editor.as_str())
        }
        ExternalLauncherError::ExternalLauncherCommandNotFoundError(error) => {
            format!("Editor command not found: {}", error.command)
        }
        ExternalLauncherError::ExternalLauncherBrowserSpawnError(error) => format!(
            "Failed to launch browser target '{}' with '{}'",
            error.target,
            std::iter::once(error.command.as_str())
                .chain(error.args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        ExternalLauncherError::ExternalLauncherEditorSpawnError(error) => format!(
            "Failed to launch '{}' in {} with '{}'",
            error.target,
            error.editor.as_str(),
            std::iter::once(error.command.as_str())
                .chain(error.args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ")
        ),
    }
}

#[cfg(test)]
mod tests;
