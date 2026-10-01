//! Server configuration and paths (`apps/server/src/config.ts`, `apps/server/src/cli/config.ts`,
//! `os-jank.ts` `resolveBaseDir`).
//!
//! Precedence for every setting is: CLI flag, then `T3CODE_*` environment variable, then the
//! default (the desktop bootstrap envelope, `--bootstrap-fd`, is not supported: zenith never
//! uses it). Environment values are parsed like Effect `Config`: booleans accept
//! `true|yes|on|1|y` / `false|no|off|0|n`, ports are integers in 1..=65535, log levels are the
//! exact Effect literals, and a present-but-invalid value is an error (not a silent default).

use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};

use crate::paths::{expand_home_path, home_dir, resolve_path};

/// `DEFAULT_PORT`.
pub const DEFAULT_PORT: u16 = 3773;

/// `RuntimeMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeMode {
    Web,
    Desktop,
}

impl RuntimeMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "web" => Some(Self::Web),
            "desktop" => Some(Self::Desktop),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Desktop => "desktop",
        }
    }
}

/// `StartupPresentation`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StartupPresentation {
    #[default]
    Browser,
    Headless,
}

/// Effect `LogLevel` literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    All,
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
    None,
}

impl LogLevel {
    /// Exact Effect literal (`"All" | "Fatal" | "Error" | "Warn" | "Info" | "Debug" | "Trace" | "None"`).
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "All" => Self::All,
            "Fatal" => Self::Fatal,
            "Error" => Self::Error,
            "Warn" => Self::Warn,
            "Info" => Self::Info,
            "Debug" => Self::Debug,
            "Trace" => Self::Trace,
            "None" => Self::None,
            _ => return None,
        })
    }
}

/// `ServerDerivedPaths`: every file and directory under the base dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerDerivedPaths {
    pub state_dir: PathBuf,
    pub db_path: PathBuf,
    pub keybindings_config_path: PathBuf,
    pub settings_path: PathBuf,
    pub environment_themes_dir: PathBuf,
    pub provider_status_cache_dir: PathBuf,
    pub worktrees_dir: PathBuf,
    pub attachments_dir: PathBuf,
    pub browser_artifacts_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub server_trace_path: PathBuf,
    pub provider_logs_dir: PathBuf,
    pub provider_event_log_path: PathBuf,
    pub terminal_logs_dir: PathBuf,
    pub anonymous_id_path: PathBuf,
    pub environment_id_path: PathBuf,
    pub server_runtime_state_path: PathBuf,
    pub secrets_dir: PathBuf,
}

/// `deriveServerPaths`. The state dir is `<baseDir>/userdata`, or `<baseDir>/dev` when a dev URL
/// is set and the base dir was not given explicitly (flag or `T3CODE_HOME`).
pub fn derive_server_paths(base_dir: &Path, dev_url: Option<&str>, base_dir_is_explicit: bool) -> ServerDerivedPaths {
    let state_dir = base_dir.join(if dev_url.is_some() && !base_dir_is_explicit { "dev" } else { "userdata" });
    let logs_dir = state_dir.join("logs");
    let provider_logs_dir = logs_dir.join("provider");
    ServerDerivedPaths {
        db_path: state_dir.join("state.sqlite"),
        keybindings_config_path: state_dir.join("keybindings.json"),
        settings_path: state_dir.join("settings.json"),
        environment_themes_dir: state_dir.join("themes"),
        provider_status_cache_dir: base_dir.join("caches"),
        worktrees_dir: base_dir.join("worktrees"),
        attachments_dir: state_dir.join("attachments"),
        browser_artifacts_dir: state_dir.join("browser-artifacts"),
        server_trace_path: logs_dir.join("server.trace.ndjson"),
        provider_event_log_path: provider_logs_dir.join("events.log"),
        terminal_logs_dir: logs_dir.join("terminals"),
        anonymous_id_path: state_dir.join("anonymous-id"),
        environment_id_path: state_dir.join("environment-id"),
        server_runtime_state_path: state_dir.join("server-runtime.json"),
        secrets_dir: state_dir.join("secrets"),
        provider_logs_dir,
        logs_dir,
        state_dir,
    }
}

/// `ensureServerDirectories` (the directory part). The TS function also sweeps expired pending
/// attachment uploads; that sweep belongs to the attachment store (orchestration, WP-08), which
/// must run it right after this at startup.
pub async fn ensure_server_directories(paths: &ServerDerivedPaths) -> std::io::Result<()> {
    let parent = |path: &Path| path.parent().map(Path::to_path_buf).unwrap_or_default();
    let directories = [
        paths.state_dir.clone(),
        paths.logs_dir.clone(),
        paths.provider_logs_dir.clone(),
        paths.terminal_logs_dir.clone(),
        paths.attachments_dir.clone(),
        paths.worktrees_dir.clone(),
        parent(&paths.keybindings_config_path),
        parent(&paths.settings_path),
        paths.provider_status_cache_dir.clone(),
        parent(&paths.anonymous_id_path),
        parent(&paths.server_runtime_state_path),
    ];
    for directory in directories {
        tokio::fs::create_dir_all(&directory).await?;
    }
    Ok(())
}

/// `resolveBaseDir`: blank means `~/.zenith/code` (zenith never shares state with an upstream
/// T3 Code install in `~/.t3`); anything else is `~`-expanded and resolved to an absolute path.
pub fn resolve_base_dir(raw: Option<&str>) -> PathBuf {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        None => home_dir().join(".zenith").join("code"),
        Some(value) => resolve_path(&expand_home_path(value)),
    }
}

/// Serialize a URL the way JS `new URL(s).toString()` does (WHATWG), or `None` if it does not
/// parse. `http://localhost:5173` becomes `http://localhost:5173/`.
pub fn normalize_url(value: &str) -> Option<String> {
    url::Url::parse(value).ok().map(|url| url.to_string())
}

/// A present environment variable that does not decode.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Invalid value for {name}: {reason}")]
pub struct ConfigError {
    pub name: String,
    pub reason: String,
}

fn config_error(name: &str, reason: impl Into<String>) -> ConfigError {
    ConfigError {
        name: name.to_owned(),
        reason: reason.into(),
    }
}

/// Where environment variables come from (the process, or a map in tests).
pub trait EnvSource {
    fn var(&self, name: &str) -> Option<String>;
}

/// The process environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

impl EnvSource for HashMap<String, String> {
    fn var(&self, name: &str) -> Option<String> {
        self.get(name).cloned()
    }
}

impl EnvSource for HashMap<&str, &str> {
    fn var(&self, name: &str) -> Option<String> {
        self.get(name).map(|value| (*value).to_owned())
    }
}

/// Effect `Config.Boolean` (`Schema.BooleanLiterals`).
pub fn parse_config_bool(value: &str) -> Option<bool> {
    match value {
        "true" | "yes" | "on" | "1" | "y" => Some(true),
        "false" | "no" | "off" | "0" | "n" => Some(false),
        _ => None,
    }
}

/// Effect `Config.Int`: a JS number string that is an integer.
pub fn parse_config_int(value: &str) -> Option<i64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(int) = trimmed.parse::<i64>() {
        return Some(int);
    }
    let float: f64 = trimmed.parse().ok()?;
    (float.is_finite() && float.fract() == 0.0 && float.abs() < 9.007_199_254_740_992e15).then_some(float as i64)
}

/// Effect `Config.Port`: an integer in 1..=65535.
pub fn parse_config_port(value: &str) -> Option<u16> {
    parse_config_int(value).filter(|port| (1..=65_535).contains(port)).map(|port| port as u16)
}

/// `EnvServerConfig`: the `T3CODE_*` variables (Appendix C of the plan).
#[derive(Debug, Clone, PartialEq)]
pub struct EnvServerConfig {
    pub log_level: LogLevel,
    pub trace_min_level: LogLevel,
    pub trace_timing_enabled: bool,
    pub trace_file: Option<String>,
    pub trace_max_bytes: i64,
    pub trace_max_files: i64,
    pub trace_batch_window_ms: i64,
    pub otlp_traces_url: Option<String>,
    pub otlp_metrics_url: Option<String>,
    pub otlp_logs_url: Option<String>,
    pub otlp_export_interval_ms: i64,
    /// `T3CODE_OTLP_HEADERS`, unparsed (the telemetry crate decodes it).
    pub otlp_headers: Option<String>,
    /// `T3CODE_OTLP_PROTOCOL`, default `"http/json"`.
    pub otlp_protocol: String,
    pub mode: Option<RuntimeMode>,
    pub port: Option<u16>,
    pub host: Option<String>,
    pub t3_home: Option<String>,
    /// `VITE_DEV_SERVER_URL`, serialized like `URL.toString()`.
    pub dev_url: Option<String>,
    pub dev_allowed_origins: Vec<String>,
    pub no_browser: Option<bool>,
    pub auto_bootstrap_project_from_cwd: Option<bool>,
    pub log_web_socket_events: Option<bool>,
    pub tailscale_serve_enabled: Option<bool>,
    pub tailscale_serve_port: Option<u16>,
}

impl EnvServerConfig {
    /// Read and validate every variable.
    pub fn from_env(env: &dyn EnvSource) -> Result<Self, ConfigError> {
        let string = |name: &str| env.var(name);
        let bool_var = |name: &str| -> Result<Option<bool>, ConfigError> {
            env.var(name)
                .map(|value| parse_config_bool(&value).ok_or_else(|| config_error(name, "expected a boolean")))
                .transpose()
        };
        let int_var = |name: &str, default: i64| -> Result<i64, ConfigError> {
            env.var(name)
                .map(|value| parse_config_int(&value).ok_or_else(|| config_error(name, "expected an integer")))
                .transpose()
                .map(|value| value.unwrap_or(default))
        };
        let port_var = |name: &str| -> Result<Option<u16>, ConfigError> {
            env.var(name)
                .map(|value| parse_config_port(&value).ok_or_else(|| config_error(name, "expected a port (1-65535)")))
                .transpose()
        };
        let level_var = |name: &str| -> Result<LogLevel, ConfigError> {
            env.var(name)
                .map(|value| LogLevel::parse(&value).ok_or_else(|| config_error(name, "expected a log level")))
                .transpose()
                .map(|value| value.unwrap_or(LogLevel::Info))
        };
        Ok(Self {
            log_level: level_var("T3CODE_LOG_LEVEL")?,
            trace_min_level: level_var("T3CODE_TRACE_MIN_LEVEL")?,
            trace_timing_enabled: bool_var("T3CODE_TRACE_TIMING_ENABLED")?.unwrap_or(true),
            trace_file: string("T3CODE_TRACE_FILE"),
            trace_max_bytes: int_var("T3CODE_TRACE_MAX_BYTES", 10 * 1024 * 1024)?,
            trace_max_files: int_var("T3CODE_TRACE_MAX_FILES", 10)?,
            trace_batch_window_ms: int_var("T3CODE_TRACE_BATCH_WINDOW_MS", 1_000)?,
            otlp_traces_url: string("T3CODE_OTLP_TRACES_URL"),
            otlp_metrics_url: string("T3CODE_OTLP_METRICS_URL"),
            otlp_logs_url: string("T3CODE_OTLP_LOGS_URL"),
            otlp_export_interval_ms: int_var("T3CODE_OTLP_EXPORT_INTERVAL_MS", 10_000)?,
            otlp_headers: string("T3CODE_OTLP_HEADERS"),
            otlp_protocol: string("T3CODE_OTLP_PROTOCOL").unwrap_or_else(|| "http/json".to_owned()),
            mode: env
                .var("T3CODE_MODE")
                .map(|value| RuntimeMode::parse(&value).ok_or_else(|| config_error("T3CODE_MODE", "expected web or desktop")))
                .transpose()?,
            port: port_var("T3CODE_PORT")?,
            host: string("T3CODE_HOST"),
            t3_home: string("T3CODE_HOME"),
            dev_url: env
                .var("VITE_DEV_SERVER_URL")
                .map(|value| normalize_url(&value).ok_or_else(|| config_error("VITE_DEV_SERVER_URL", "expected a URL")))
                .transpose()?,
            dev_allowed_origins: string("T3CODE_DEV_ALLOWED_ORIGINS")
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(str::to_owned)
                .collect(),
            no_browser: bool_var("T3CODE_NO_BROWSER")?,
            auto_bootstrap_project_from_cwd: bool_var("T3CODE_AUTO_BOOTSTRAP_PROJECT_FROM_CWD")?,
            log_web_socket_events: bool_var("T3CODE_LOG_WS_EVENTS")?,
            tailscale_serve_enabled: bool_var("T3CODE_TAILSCALE_SERVE")?,
            tailscale_serve_port: port_var("T3CODE_TAILSCALE_SERVE_PORT")?,
        })
    }
}

/// `T3CODE_DEV_AUTH_TOKEN`: trimmed; empty means unset; otherwise at least 32 characters.
pub fn read_dev_auth_token(env: &dyn EnvSource) -> Result<Option<String>, ConfigError> {
    let Some(raw) = env.var("T3CODE_DEV_AUTH_TOKEN") else {
        return Ok(None);
    };
    let token = raw.trim();
    if token.is_empty() {
        return Ok(None);
    }
    if token.chars().count() < 32 {
        return Err(config_error(
            "T3CODE_DEV_AUTH_TOKEN",
            "T3CODE_DEV_AUTH_TOKEN must contain at least 32 characters.",
        ));
    }
    Ok(Some(token.to_owned()))
}

/// `CliServerFlags`: what the `serve` command line said (all optional).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliServerFlags {
    pub mode: Option<RuntimeMode>,
    pub port: Option<u16>,
    pub host: Option<String>,
    pub base_dir: Option<String>,
    pub cwd: Option<String>,
    /// Must parse as a URL (the CLI rejects it otherwise).
    pub dev_url: Option<String>,
    pub no_browser: Option<bool>,
    pub auto_bootstrap_project_from_cwd: Option<bool>,
    pub log_web_socket_events: Option<bool>,
    pub tailscale_serve_enabled: Option<bool>,
    pub tailscale_serve_port: Option<u16>,
    /// `--static-dir` (Rust addition, plan §10).
    pub static_dir: Option<String>,
}

/// Options of `resolveServerConfig`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResolveServerConfigOptions {
    pub startup_presentation: StartupPresentation,
    pub force_auto_bootstrap_project_from_cwd: Option<bool>,
}

/// The resolved `ServerConfig` service value.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerConfig {
    pub paths: ServerDerivedPaths,
    pub log_level: LogLevel,
    pub trace_min_level: LogLevel,
    pub trace_timing_enabled: bool,
    pub trace_batch_window_ms: i64,
    pub trace_max_bytes: i64,
    pub trace_max_files: i64,
    /// OTLP endpoints as far as zc-core resolves them: the `T3CODE_OTLP_*_URL` variable, else the
    /// `observability.*` value persisted in settings.json. The standard `OTEL_*` variables (which
    /// can also disable export) are layered on by the telemetry crate (WP-31).
    pub otlp_traces_url: Option<String>,
    pub otlp_metrics_url: Option<String>,
    pub otlp_logs_url: Option<String>,
    pub otlp_export_interval_ms: i64,
    pub otlp_headers: Option<String>,
    pub otlp_protocol: String,
    pub mode: RuntimeMode,
    pub port: u16,
    pub host: Option<String>,
    pub cwd: PathBuf,
    pub base_dir: PathBuf,
    pub static_dir: Option<PathBuf>,
    pub dev_url: Option<String>,
    pub dev_auth_token: Option<String>,
    pub dev_allowed_origins: Vec<String>,
    pub no_browser: bool,
    pub startup_presentation: StartupPresentation,
    pub auto_bootstrap_project_from_cwd: bool,
    pub log_web_socket_events: bool,
    pub tailscale_serve_enabled: bool,
    pub tailscale_serve_port: u16,
}

/// `resolveServerConfig`: flags over environment over defaults; creates the cwd and the server
/// directories, picks a free port when none was given (web mode).
pub async fn resolve_server_config(
    flags: &CliServerFlags,
    cli_log_level: Option<LogLevel>,
    options: ResolveServerConfigOptions,
    env_source: &dyn EnvSource,
) -> Result<ServerConfig, ResolveConfigError> {
    let env = EnvServerConfig::from_env(env_source)?;
    let mode = flags.mode.or(env.mode).unwrap_or(RuntimeMode::Web);
    let port = match flags.port.or(env.port) {
        Some(port) => port,
        None if mode == RuntimeMode::Desktop => DEFAULT_PORT,
        None => find_available_port(DEFAULT_PORT).map_err(ResolveConfigError::Port)?,
    };
    let dev_url = match &flags.dev_url {
        Some(raw) => Some(normalize_url(raw).ok_or_else(|| config_error("--dev-url", "expected a URL"))?),
        None => env.dev_url.clone(),
    };
    let dev_auth_token = if mode == RuntimeMode::Web && dev_url.is_some() {
        read_dev_auth_token(env_source)?
    } else {
        None
    };
    let explicit_base_dir = flags.base_dir.clone().or_else(|| env.t3_home.clone()).filter(|value| !value.trim().is_empty());
    let base_dir = resolve_base_dir(explicit_base_dir.as_deref());
    let raw_cwd = flags.cwd.clone().unwrap_or_else(|| {
        std::env::current_dir()
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_owned())
    });
    let cwd = resolve_path(&expand_home_path(raw_cwd.trim()));
    tokio::fs::create_dir_all(&cwd).await.map_err(ResolveConfigError::Io)?;
    let mut paths = derive_server_paths(&base_dir, dev_url.as_deref(), explicit_base_dir.is_some());
    ensure_server_directories(&paths).await.map_err(ResolveConfigError::Io)?;
    let persisted = read_persisted_observability_settings(&paths.settings_path).await;
    if let Some(trace_file) = env.trace_file.as_deref().filter(|value| !value.is_empty()) {
        paths.server_trace_path = PathBuf::from(trace_file);
    }
    if let Some(parent) = paths.server_trace_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent).await.map_err(ResolveConfigError::Io)?;
    }
    let headless = options.startup_presentation == StartupPresentation::Headless;
    let no_browser = (headless.then_some(true))
        .or(flags.no_browser)
        .or(env.no_browser)
        .unwrap_or(mode == RuntimeMode::Desktop);
    let auto_bootstrap_project_from_cwd = options
        .force_auto_bootstrap_project_from_cwd
        .or(headless.then_some(false))
        .or(flags.auto_bootstrap_project_from_cwd)
        .or(env.auto_bootstrap_project_from_cwd)
        .unwrap_or(mode == RuntimeMode::Web);
    let log_web_socket_events = flags.log_web_socket_events.or(env.log_web_socket_events).unwrap_or(dev_url.is_some());
    let tailscale_serve_enabled = flags.tailscale_serve_enabled.or(env.tailscale_serve_enabled).unwrap_or(false);
    let tailscale_serve_port = flags.tailscale_serve_port.or(env.tailscale_serve_port).unwrap_or(443);
    let static_dir = if dev_url.is_some() {
        None
    } else {
        let explicit = flags
            .static_dir
            .clone()
            .or_else(|| env_source.var("ZENITH_CODE_STATIC_DIR"))
            .filter(|value| !value.trim().is_empty())
            .map(|value| resolve_path(&expand_home_path(value.trim())));
        let exe_dir = std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf));
        resolve_static_dir(explicit.as_deref(), exe_dir.as_deref())
    };
    let host = flags
        .host
        .clone()
        .or_else(|| env.host.clone())
        .or_else(|| (mode == RuntimeMode::Desktop).then(|| "127.0.0.1".to_owned()));
    let blank_as_unset = |value: Option<String>| value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());

    Ok(ServerConfig {
        log_level: cli_log_level.unwrap_or(env.log_level),
        trace_min_level: env.trace_min_level,
        trace_timing_enabled: env.trace_timing_enabled,
        trace_batch_window_ms: env.trace_batch_window_ms,
        trace_max_bytes: env.trace_max_bytes,
        trace_max_files: env.trace_max_files,
        otlp_traces_url: blank_as_unset(env.otlp_traces_url).or(persisted.otlp_traces_url),
        otlp_metrics_url: blank_as_unset(env.otlp_metrics_url).or(persisted.otlp_metrics_url),
        otlp_logs_url: blank_as_unset(env.otlp_logs_url).or(persisted.otlp_logs_url),
        otlp_export_interval_ms: env.otlp_export_interval_ms,
        otlp_headers: env.otlp_headers,
        otlp_protocol: env.otlp_protocol,
        mode,
        port,
        host,
        cwd,
        base_dir,
        static_dir,
        dev_url,
        dev_auth_token,
        dev_allowed_origins: env.dev_allowed_origins,
        no_browser,
        startup_presentation: options.startup_presentation,
        auto_bootstrap_project_from_cwd,
        log_web_socket_events,
        tailscale_serve_enabled,
        tailscale_serve_port,
        paths,
    })
}

/// Why [`resolve_server_config`] failed.
#[derive(Debug, thiserror::Error)]
pub enum ResolveConfigError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("Failed to reserve a port: {0}")]
    Port(#[source] std::io::Error),
    #[error(transparent)]
    Io(std::io::Error),
}

/// The `observability.*` URLs persisted in settings.json (`parsePersistedServerObservabilitySettings`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersistedObservabilitySettings {
    pub otlp_traces_url: Option<String>,
    pub otlp_metrics_url: Option<String>,
    pub otlp_logs_url: Option<String>,
}

/// Read the persisted observability URLs. The TS version decodes the whole `ServerSettings`
/// schema first and ignores the file when any field is invalid; zc-core only checks that the
/// file is lenient JSON (the full schema lives in zc-settings).
pub async fn read_persisted_observability_settings(settings_path: &Path) -> PersistedObservabilitySettings {
    let Ok(raw) = tokio::fs::read_to_string(settings_path).await else {
        return PersistedObservabilitySettings::default();
    };
    parse_persisted_observability_settings(&raw)
}

/// Pure part of [`read_persisted_observability_settings`].
pub fn parse_persisted_observability_settings(raw: &str) -> PersistedObservabilitySettings {
    let Ok(value) = crate::lenient_json::parse_lenient_json(raw) else {
        return PersistedObservabilitySettings::default();
    };
    let field = |name: &str| {
        value
            .get("observability")
            .and_then(|o| o.get(name))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    PersistedObservabilitySettings {
        otlp_traces_url: field("otlpTracesUrl"),
        otlp_metrics_url: field("otlpMetricsUrl"),
        otlp_logs_url: field("otlpLogsUrl"),
    }
}

/// Where the built web client can be. In order: the explicit directory (`--static-dir`,
/// `ZENITH_CODE_STATIC_DIR`), `<exe dir>/client`, then `code/apps/server/dist/client` and
/// `code/apps/web/dist` in the repository holding the binary (searched upward from the exe
/// directory) and in the repository this crate was built from.
pub fn static_dir_candidates(explicit: Option<&Path>, exe_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(explicit) = explicit {
        candidates.push(explicit.to_path_buf());
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(exe_dir) = exe_dir {
        candidates.push(exe_dir.join("client"));
        roots.extend(exe_dir.ancestors().take(5).map(Path::to_path_buf));
    }
    // crates/zenith-code/crates/zc-core → repository root.
    roots.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.."));
    for root in roots {
        for relative in ["code/apps/server/dist/client", "code/apps/web/dist"] {
            let candidate = crate::paths::normalize_lexically(&root.join(relative));
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
    }
    candidates
}

/// `resolveStaticDir`: the first candidate holding an `index.html`.
pub fn resolve_static_dir(explicit: Option<&Path>, exe_dir: Option<&Path>) -> Option<PathBuf> {
    static_dir_candidates(explicit, exe_dir)
        .into_iter()
        .find(|dir| dir.join("index.html").is_file())
}

/// `NetService.findAvailablePort`: the preferred port when both `127.0.0.1` and `::1` can bind it
/// (an address the host lacks counts as available), otherwise an ephemeral loopback port.
pub fn find_available_port(preferred: u16) -> std::io::Result<u16> {
    if preferred > 0 && is_port_available_on_loopback(preferred) {
        return Ok(preferred);
    }
    reserve_loopback_port()
}

/// `isPortAvailableOnLoopback`.
pub fn is_port_available_on_loopback(port: u16) -> bool {
    can_listen(SocketAddr::from((Ipv4Addr::LOCALHOST, port))) && can_listen(SocketAddr::from((Ipv6Addr::LOCALHOST, port)))
}

fn can_listen(address: SocketAddr) -> bool {
    match TcpListener::bind(address) {
        Ok(_) => true,
        Err(error) => error.raw_os_error() == Some(libc::EADDRNOTAVAIL),
    }
}

/// `reserveLoopbackPort`: bind `127.0.0.1:0`, read the port, release it.
pub fn reserve_loopback_port() -> std::io::Result<u16> {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
    listener.local_addr().map(|address| address.port())
}

/// `parseDurationInput` of `cli/config.ts`: `^\d+(ms|s|m|h|d|w)$` (case-insensitive) or an
/// Effect duration string such as `15 minutes`. Returns milliseconds.
pub fn parse_duration_input(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    let digits_end = lower.find(|c: char| !c.is_ascii_digit()).unwrap_or(lower.len());
    if digits_end > 0 && digits_end < lower.len() {
        let amount: u64 = lower[..digits_end].parse().ok()?;
        let unit_ms = match &lower[digits_end..] {
            "ms" => Some(1),
            "s" => Some(1_000),
            "m" => Some(60_000),
            "h" => Some(3_600_000),
            "d" => Some(86_400_000),
            "w" => Some(604_800_000),
            _ => None,
        };
        if let Some(unit_ms) = unit_ms {
            return amount.checked_mul(unit_ms);
        }
    }
    // Effect `Duration.fromInput` string form: "<number> <unit>".
    let mut parts = trimmed.split_whitespace();
    let (amount, unit) = (parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let amount: f64 = amount.parse().ok()?;
    if !amount.is_finite() || amount < 0.0 {
        return None;
    }
    let unit_ms: f64 = match unit {
        "nano" | "nanos" => 1e-6,
        "micro" | "micros" => 1e-3,
        "milli" | "millis" => 1.0,
        "second" | "seconds" => 1_000.0,
        "minute" | "minutes" => 60_000.0,
        "hour" | "hours" => 3_600_000.0,
        "day" | "days" => 86_400_000.0,
        "week" | "weeks" => 604_800_000.0,
        _ => return None,
    };
    Some((amount * unit_ms) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_userdata_paths() {
        let paths = derive_server_paths(Path::new("/b"), None, false);
        assert_eq!(paths.state_dir, PathBuf::from("/b/userdata"));
        assert_eq!(paths.db_path, PathBuf::from("/b/userdata/state.sqlite"));
        assert_eq!(paths.keybindings_config_path, PathBuf::from("/b/userdata/keybindings.json"));
        assert_eq!(paths.settings_path, PathBuf::from("/b/userdata/settings.json"));
        assert_eq!(paths.environment_themes_dir, PathBuf::from("/b/userdata/themes"));
        assert_eq!(paths.provider_status_cache_dir, PathBuf::from("/b/caches"));
        assert_eq!(paths.worktrees_dir, PathBuf::from("/b/worktrees"));
        assert_eq!(paths.attachments_dir, PathBuf::from("/b/userdata/attachments"));
        assert_eq!(paths.browser_artifacts_dir, PathBuf::from("/b/userdata/browser-artifacts"));
        assert_eq!(paths.logs_dir, PathBuf::from("/b/userdata/logs"));
        assert_eq!(paths.server_trace_path, PathBuf::from("/b/userdata/logs/server.trace.ndjson"));
        assert_eq!(paths.provider_logs_dir, PathBuf::from("/b/userdata/logs/provider"));
        assert_eq!(paths.provider_event_log_path, PathBuf::from("/b/userdata/logs/provider/events.log"));
        assert_eq!(paths.terminal_logs_dir, PathBuf::from("/b/userdata/logs/terminals"));
        assert_eq!(paths.anonymous_id_path, PathBuf::from("/b/userdata/anonymous-id"));
        assert_eq!(paths.environment_id_path, PathBuf::from("/b/userdata/environment-id"));
        assert_eq!(paths.server_runtime_state_path, PathBuf::from("/b/userdata/server-runtime.json"));
        assert_eq!(paths.secrets_dir, PathBuf::from("/b/userdata/secrets"));
    }

    #[test]
    fn dev_url_uses_the_dev_state_dir_only_for_an_implicit_base_dir() {
        let dev = Some("http://localhost:5173/");
        assert_eq!(derive_server_paths(Path::new("/b"), dev, false).state_dir, PathBuf::from("/b/dev"));
        assert_eq!(derive_server_paths(Path::new("/b"), dev, true).state_dir, PathBuf::from("/b/userdata"));
        assert_eq!(derive_server_paths(Path::new("/b"), dev, false).worktrees_dir, PathBuf::from("/b/worktrees"));
    }

    #[test]
    fn base_dir_defaults_to_zenith_code() {
        let default = resolve_base_dir(None);
        assert!(default.ends_with(".zenith/code"));
        assert_eq!(resolve_base_dir(Some("   ")), default);
        assert_eq!(resolve_base_dir(Some(" /tmp/x/../y ")), PathBuf::from("/tmp/y"));
        assert_eq!(resolve_base_dir(Some("~/state")), home_dir().join("state"));
        assert!(resolve_base_dir(Some("relative")).is_absolute());
    }

    #[test]
    fn parses_env_like_effect_config() {
        assert_eq!(parse_config_bool("yes"), Some(true));
        assert_eq!(parse_config_bool("y"), Some(true));
        assert_eq!(parse_config_bool("off"), Some(false));
        assert_eq!(parse_config_bool("TRUE"), None);
        assert_eq!(parse_config_port("3773"), Some(3773));
        assert_eq!(parse_config_port("0"), None);
        assert_eq!(parse_config_port("65536"), None);
        assert_eq!(parse_config_int("10.0"), Some(10));
        assert_eq!(parse_config_int("1.5"), None);
        assert_eq!(LogLevel::parse("Debug"), Some(LogLevel::Debug));
        assert_eq!(LogLevel::parse("debug"), None);
    }

    #[test]
    fn env_server_config_defaults_and_errors() {
        let empty: HashMap<&str, &str> = HashMap::new();
        let env = EnvServerConfig::from_env(&empty).unwrap();
        assert_eq!(env.log_level, LogLevel::Info);
        assert!(env.trace_timing_enabled);
        assert_eq!(env.trace_max_bytes, 10 * 1024 * 1024);
        assert_eq!(env.trace_max_files, 10);
        assert_eq!(env.trace_batch_window_ms, 1_000);
        assert_eq!(env.otlp_export_interval_ms, 10_000);
        assert_eq!(env.otlp_protocol, "http/json");
        assert_eq!(env.mode, None);

        let set: HashMap<&str, &str> = HashMap::from([
            ("T3CODE_MODE", "desktop"),
            ("T3CODE_PORT", "4000"),
            ("T3CODE_NO_BROWSER", "1"),
            ("VITE_DEV_SERVER_URL", "http://localhost:5173"),
            ("T3CODE_DEV_ALLOWED_ORIGINS", " http://a , ,http://b"),
        ]);
        let env = EnvServerConfig::from_env(&set).unwrap();
        assert_eq!(env.mode, Some(RuntimeMode::Desktop));
        assert_eq!(env.port, Some(4000));
        assert_eq!(env.no_browser, Some(true));
        assert_eq!(env.dev_url.as_deref(), Some("http://localhost:5173/"));
        assert_eq!(env.dev_allowed_origins, vec!["http://a".to_owned(), "http://b".to_owned()]);

        let bad: HashMap<&str, &str> = HashMap::from([("T3CODE_PORT", "abc")]);
        assert_eq!(EnvServerConfig::from_env(&bad).unwrap_err().name, "T3CODE_PORT");
        let bad: HashMap<&str, &str> = HashMap::from([("T3CODE_NO_BROWSER", "maybe")]);
        assert!(EnvServerConfig::from_env(&bad).is_err());
    }

    #[test]
    fn dev_auth_token_rules() {
        let short: HashMap<&str, &str> = HashMap::from([("T3CODE_DEV_AUTH_TOKEN", " short ")]);
        assert!(read_dev_auth_token(&short).is_err());
        let blank: HashMap<&str, &str> = HashMap::from([("T3CODE_DEV_AUTH_TOKEN", "  ")]);
        assert_eq!(read_dev_auth_token(&blank).unwrap(), None);
        let long = "x".repeat(32);
        let ok: HashMap<&str, &str> = HashMap::from([("T3CODE_DEV_AUTH_TOKEN", long.as_str())]);
        assert_eq!(read_dev_auth_token(&ok).unwrap().as_deref(), Some(long.as_str()));
    }

    #[tokio::test]
    async fn resolves_flags_over_env_over_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let env: HashMap<&str, &str> = HashMap::from([("T3CODE_PORT", "4100"), ("T3CODE_HOST", "0.0.0.0"), ("T3CODE_HOME", "/should/lose/to/the/flag")]);
        let flags = CliServerFlags {
            port: Some(4200),
            base_dir: Some(base.to_string_lossy().into_owned()),
            cwd: Some(dir.path().join("work").to_string_lossy().into_owned()),
            ..Default::default()
        };
        let config = resolve_server_config(&flags, None, Default::default(), &env).await.unwrap();
        assert_eq!(config.port, 4200);
        assert_eq!(config.host.as_deref(), Some("0.0.0.0"));
        assert_eq!(config.mode, RuntimeMode::Web);
        assert_eq!(config.base_dir, base);
        assert_eq!(config.paths.state_dir, base.join("userdata"));
        assert!(config.paths.secrets_dir.parent().unwrap().is_dir());
        assert!(config.paths.terminal_logs_dir.is_dir());
        assert!(config.paths.provider_status_cache_dir.is_dir());
        assert!(config.cwd.is_dir());
        assert!(!config.no_browser);
        assert!(config.auto_bootstrap_project_from_cwd);
        assert!(!config.log_web_socket_events);
        assert_eq!(config.tailscale_serve_port, 443);

        // Desktop mode: fixed default port and loopback host, browser off.
        let desktop = CliServerFlags {
            mode: Some(RuntimeMode::Desktop),
            base_dir: Some(base.to_string_lossy().into_owned()),
            cwd: Some(dir.path().to_string_lossy().into_owned()),
            ..Default::default()
        };
        let empty: HashMap<&str, &str> = HashMap::new();
        let config = resolve_server_config(&desktop, Some(LogLevel::Debug), Default::default(), &empty)
            .await
            .unwrap();
        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.host.as_deref(), Some("127.0.0.1"));
        assert!(config.no_browser);
        assert!(!config.auto_bootstrap_project_from_cwd);
        assert_eq!(config.log_level, LogLevel::Debug);

        // Headless startup forces the browser off and bootstrap off.
        let headless = ResolveServerConfigOptions {
            startup_presentation: StartupPresentation::Headless,
            ..Default::default()
        };
        let flags = CliServerFlags {
            no_browser: Some(false),
            port: Some(4300),
            base_dir: Some(base.to_string_lossy().into_owned()),
            cwd: Some(dir.path().to_string_lossy().into_owned()),
            ..Default::default()
        };
        let config = resolve_server_config(&flags, None, headless, &empty).await.unwrap();
        assert!(config.no_browser);
        assert!(!config.auto_bootstrap_project_from_cwd);
    }

    #[tokio::test]
    async fn dev_url_with_implicit_base_dir_uses_dev_state_and_logs_ws_events() {
        let dir = tempfile::tempdir().unwrap();
        let env: HashMap<String, String> = HashMap::from([("HOME".to_owned(), dir.path().to_string_lossy().into_owned())]);
        let flags = CliServerFlags {
            port: Some(4400),
            dev_url: Some("http://localhost:5173".into()),
            // An explicit base dir keeps userdata even with a dev URL.
            base_dir: Some(dir.path().join("explicit").to_string_lossy().into_owned()),
            cwd: Some(dir.path().to_string_lossy().into_owned()),
            ..Default::default()
        };
        let config = resolve_server_config(&flags, None, Default::default(), &env).await.unwrap();
        assert_eq!(config.dev_url.as_deref(), Some("http://localhost:5173/"));
        assert!(config.log_web_socket_events);
        assert_eq!(config.static_dir, None);
        assert_eq!(config.paths.state_dir, dir.path().join("explicit/userdata"));
    }

    #[test]
    fn observability_urls_come_from_lenient_settings() {
        let parsed = parse_persisted_observability_settings(
            "{ // c\n \"observability\": { \"otlpTracesUrl\": \" http://otel:4318/v1/traces \", \"otlpLogsUrl\": \"\" }, }",
        );
        assert_eq!(parsed.otlp_traces_url.as_deref(), Some("http://otel:4318/v1/traces"));
        assert_eq!(parsed.otlp_logs_url, None);
        assert_eq!(parse_persisted_observability_settings("not json"), PersistedObservabilitySettings::default());
    }

    #[test]
    fn static_dir_prefers_explicit_then_exe_client() {
        let dir = tempfile::tempdir().unwrap();
        let explicit = dir.path().join("static");
        let exe_dir = dir.path().join("bin");
        std::fs::create_dir_all(exe_dir.join("client")).unwrap();
        std::fs::write(exe_dir.join("client/index.html"), "<html>").unwrap();
        assert_eq!(resolve_static_dir(Some(&explicit), Some(&exe_dir)), Some(exe_dir.join("client")));
        std::fs::create_dir_all(&explicit).unwrap();
        std::fs::write(explicit.join("index.html"), "<html>").unwrap();
        assert_eq!(resolve_static_dir(Some(&explicit), Some(&exe_dir)), Some(explicit));
        let candidates = static_dir_candidates(None, Some(&exe_dir));
        assert!(candidates.iter().any(|c| c.ends_with("code/apps/server/dist/client")));
    }

    #[test]
    fn finds_ports() {
        let held = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        let taken = held.local_addr().unwrap().port();
        let chosen = find_available_port(taken).unwrap();
        assert_ne!(chosen, taken);
        assert!(reserve_loopback_port().unwrap() > 0);
    }

    #[test]
    fn parses_cli_durations() {
        assert_eq!(parse_duration_input("2m"), Some(120_000));
        assert_eq!(parse_duration_input("12H"), Some(43_200_000));
        assert_eq!(parse_duration_input("250ms"), Some(250));
        assert_eq!(parse_duration_input("30d"), Some(2_592_000_000));
        assert_eq!(parse_duration_input("1w"), Some(604_800_000));
        assert_eq!(parse_duration_input("15 minutes"), Some(900_000));
        assert_eq!(parse_duration_input("1.5 hours"), Some(5_400_000));
        assert_eq!(parse_duration_input("soon"), None);
        assert_eq!(parse_duration_input(""), None);
        assert_eq!(parse_duration_input("5x"), None);
    }
}
