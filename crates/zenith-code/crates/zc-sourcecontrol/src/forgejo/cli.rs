//! `ForgejoCli.ts`: Forgejo and Gitea through `fj` (0.6+) or `tea` (0.16+).
//!
//! `fj` keeps its tokens in `keys.json` (its `directories::ProjectDirs` location); this module
//! reads them and calls the Forgejo API directly with them, letting `fj whoami` refresh OAuth
//! tokens first. `tea` is driven as a CLI (`tea api --include`). Logins are matched to remotes
//! by host, SSH alias and mount path ([`match_forgejo_login`]).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use regex::Regex;
use serde::{Serialize, Serializer};
use serde_json::{json, Value};
use zc_core::vcs_process::{VcsProcess, VcsProcessError, VcsProcessExitFailureKind, VcsProcessInput, VcsProcessOutput};

use crate::errors::{error_defect, tagged, Cause, CauseError};
use crate::http::collect_body;
use crate::provider::SourceControlProviderContext;
use crate::util::{encode_uri_component, js_trim, parse_url, url_host, url_hostname, url_origin, SharedClock};

/// `fj` or `tea`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ForgejoCommand {
    Fj,
    Tea,
}

impl ForgejoCommand {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fj => "fj",
            Self::Tea => "tea",
        }
    }
}

/// The `reason` of a [`ForgejoCliError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgejoErrorReason {
    MissingCli,
    Authentication,
    Forbidden,
    NotFound,
    RateLimit,
    InvalidResponse,
}

impl ForgejoErrorReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingCli => "missing-cli",
            Self::Authentication => "authentication",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not-found",
            Self::RateLimit => "rate-limit",
            Self::InvalidResponse => "invalid-response",
        }
    }

    fn from_status(status: u16) -> Option<Self> {
        match status {
            401 => Some(Self::Authentication),
            403 => Some(Self::Forbidden),
            404 => Some(Self::NotFound),
            429 => Some(Self::RateLimit),
            _ => None,
        }
    }
}

/// `ForgejoCliError`.
#[derive(Debug, Clone)]
pub struct ForgejoCliError {
    pub command: ForgejoCommand,
    pub cwd: String,
    pub detail: String,
    pub reason: Option<ForgejoErrorReason>,
    pub http_status: Option<u16>,
    pub cause: Option<Cause>,
}

impl ForgejoCliError {
    pub fn new(command: ForgejoCommand, cwd: &str, detail: impl Into<String>) -> Self {
        Self {
            command,
            cwd: cwd.to_owned(),
            detail: detail.into(),
            reason: None,
            http_status: None,
            cause: None,
        }
    }

    pub fn with_reason(mut self, reason: ForgejoErrorReason) -> Self {
        self.reason = Some(reason);
        self
    }

    pub fn with_cause(mut self, cause: Cause) -> Self {
        self.cause = Some(cause);
        self
    }

    /// The tagged-error message (Effect's default for a tagged error without a `message` getter
    /// is the empty string; the detail is what callers show).
    pub fn message(&self) -> String {
        String::new()
    }
}

impl std::fmt::Display for ForgejoCliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for ForgejoCliError {}

impl Serialize for ForgejoCliError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        tagged(
            "ForgejoCliError",
            json!({
                "command": self.command.as_str(),
                "cwd": self.cwd,
                "detail": self.detail,
                "reason": self.reason.map(ForgejoErrorReason::as_str),
                "httpStatus": self.http_status,
            }),
            self.cause.as_ref(),
        )
        .serialize(serializer)
    }
}

impl CauseError for ForgejoCliError {
    fn defect(&self) -> Value {
        error_defect("ForgejoCliError", self.message(), self.cause.as_ref().map(Cause::defect))
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// `ForgejoLoginSchema`: one `tea login list` entry (or an `fj` key presented the same way).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ForgejoLogin {
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid: Option<String>,
    pub user: String,
    pub default: String,
}

/// `parseForgejoLogins`: all entries, or none when any entry is malformed.
pub fn parse_forgejo_logins(raw: &str) -> Vec<ForgejoLogin> {
    serde_json::from_str::<Vec<ForgejoLogin>>(raw).unwrap_or_default()
}

/// `ForgejoKeysSchema`: `fj`'s `keys.json`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ForgejoKeys {
    /// host → token.
    pub hosts: BTreeMap<String, String>,
    /// alias → host, in file order.
    pub aliases: Vec<(String, String)>,
}

/// Decodes `keys.json` (`hosts` entries must be `{type: "Application"|"OAuth", token}`).
pub fn parse_forgejo_keys(raw: &str) -> Option<ForgejoKeys> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let map = value.as_object()?;
    let mut hosts = BTreeMap::new();
    for (host, entry) in map.get("hosts")?.as_object()? {
        let entry = entry.as_object()?;
        match entry.get("type")?.as_str()? {
            "Application" | "OAuth" => {}
            _ => return None,
        }
        hosts.insert(host.clone(), entry.get("token")?.as_str()?.to_owned());
    }
    let aliases = match map.get("aliases") {
        None => Vec::new(),
        Some(Value::Object(aliases)) => aliases
            .iter()
            .map(|(alias, target)| Some((alias.clone(), target.as_str()?.to_owned())))
            .collect::<Option<Vec<_>>>()?,
        Some(_) => return None,
    };
    Some(ForgejoKeys { hosts, aliases })
}

/// Where `fj` stores `keys.json` (`directories::ProjectDirs`, including its pre-0.6
/// organization name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgejoEnvironment {
    /// `darwin`, `win32`, `linux`, …
    pub platform: String,
    pub home: PathBuf,
    pub data_home: Option<String>,
    pub app_data: Option<String>,
}

impl ForgejoEnvironment {
    /// This process's platform, home and `XDG_DATA_HOME` / `APPDATA`.
    pub fn from_process() -> Self {
        let platform = if cfg!(target_os = "macos") {
            "darwin"
        } else if cfg!(windows) {
            "win32"
        } else {
            "linux"
        };
        Self {
            platform: platform.into(),
            home: zc_core::paths::home_dir(),
            data_home: std::env::var("XDG_DATA_HOME").ok().filter(|v| !v.is_empty()),
            app_data: std::env::var("APPDATA").ok().filter(|v| !v.is_empty()),
        }
    }

    /// `forgejoKeysPaths`.
    pub fn keys_paths(&self) -> Vec<PathBuf> {
        match self.platform.as_str() {
            "darwin" => ["forgejo-cli", "Cyborus"]
                .iter()
                .map(|organization| {
                    self.home
                        .join("Library")
                        .join("Application Support")
                        .join(format!("{organization}.forgejo-cli"))
                        .join("keys.json")
                })
                .collect(),
            "win32" => ["forgejo-cli", "Cyborus"]
                .iter()
                .map(|organization| {
                    let base = self.app_data.as_ref().map_or_else(|| self.home.join("AppData").join("Roaming"), PathBuf::from);
                    base.join(organization).join("forgejo-cli").join("data").join("keys.json")
                })
                .collect(),
            _ => {
                let base = match &self.data_home {
                    Some(data_home) if Path::new(data_home).is_absolute() => PathBuf::from(data_home),
                    _ => self.home.join(".local").join("share"),
                };
                vec![base.join("forgejo-cli").join("keys.json")]
            }
        }
    }
}

/// `parseForgejoRemote`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgejoRemote {
    pub host: String,
    pub hostname: String,
    pub ssh: bool,
    pub path: String,
}

/// `parseForgejoRemote`: URLs and SCP-style SSH remotes.
pub fn parse_forgejo_remote(value: &str) -> Option<ForgejoRemote> {
    static SCHEME: OnceLock<Regex> = OnceLock::new();
    static SCP: OnceLock<Regex> = OnceLock::new();
    if SCHEME
        .get_or_init(|| Regex::new(r"(?i)^(?:https?|ssh)://").expect("valid regex"))
        .is_match(value)
    {
        let url = parse_url(value)?;
        let path = url.path().trim_matches('/');
        return Some(ForgejoRemote {
            host: url_host(&url).to_lowercase(),
            hostname: url_hostname(&url).to_lowercase(),
            ssh: url.scheme() == "ssh",
            path: path.strip_suffix(".git").unwrap_or(path).to_owned(),
        });
    }
    // SCP remotes may omit the username.
    let captures = SCP
        .get_or_init(|| Regex::new(r"^(?:[^@/]+@)?([^:/]+):([^/].*)$").expect("valid regex"))
        .captures(value)?;
    let host = captures.get(1)?.as_str().to_lowercase();
    let path = captures.get(2)?.as_str();
    Some(ForgejoRemote {
        hostname: host.clone(),
        host,
        ssh: true,
        path: path.strip_suffix(".git").unwrap_or(path).to_owned(),
    })
}

/// `matchForgejoLogin`: the single login serving `remote`, or the default one among logins of
/// one server; `None` when ambiguous.
pub fn match_forgejo_login(logins: &[ForgejoLogin], remote: &ForgejoRemote, requested_host: Option<&str>, host_only: bool) -> Option<ForgejoLogin> {
    let mut matches: Vec<ForgejoLogin> = Vec::new();
    for login in logins {
        let Some(url) = parse_forgejo_remote(&login.url) else {
            continue;
        };
        if let Some(requested) = requested_host {
            if url.host != requested.to_lowercase() {
                continue;
            }
        }
        let ok = if remote.ssh {
            let ssh_host = login.ssh_host.as_ref().map(|h| h.to_lowercase());
            ssh_host.as_deref() == Some(remote.host.as_str()) || ssh_host.as_deref() == Some(remote.hostname.as_str()) || url.hostname == remote.hostname
        } else {
            url.host == remote.host
                && ((host_only && remote.path.is_empty())
                    || url.path.is_empty()
                    || remote.path == url.path
                    || remote.path.starts_with(&format!("{}/", url.path)))
        };
        if !ok {
            continue;
        }
        // `new Map(… [login.name, login])`: first position, last value.
        match matches.iter_mut().find(|existing| existing.name == login.name) {
            Some(existing) => *existing = login.clone(),
            None => matches.push(login.clone()),
        }
    }
    if matches.len() == 1 {
        return matches.pop();
    }
    let first_url = matches.first().map(|login| login.url.clone());
    if matches.iter().all(|login| Some(&login.url) == first_url.as_ref()) {
        return matches.into_iter().find(|login| login.default == "true");
    }
    None
}

/// `ForgejoRepositoryInput`.
#[derive(Debug, Clone, Default)]
pub struct ForgejoRepositoryInput {
    pub cwd: String,
    pub context: Option<SourceControlProviderContext>,
    pub repository: Option<String>,
    pub reference: Option<String>,
    pub host: Option<String>,
}

/// `ForgejoRepository`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgejoRepository {
    pub command: ForgejoCommand,
    pub login: String,
    pub repository: String,
    pub base_url: String,
}

/// `ForgejoApiInput`.
#[derive(Debug, Clone, Default)]
pub struct ForgejoApiInput {
    pub target: ForgejoRepositoryInput,
    pub path: String,
    /// `GET` when `None`.
    pub method: Option<String>,
    pub body: Option<Value>,
}

/// `execute` input.
#[derive(Debug, Clone)]
pub struct ForgejoExecuteInput {
    pub command: ForgejoCommand,
    pub cwd: String,
    pub args: Vec<String>,
    pub stdin: Option<String>,
    pub timeout_ms: Option<u64>,
    pub max_output_bytes: Option<usize>,
}

impl ForgejoExecuteInput {
    pub fn new<I, S>(command: ForgejoCommand, cwd: &str, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            command,
            cwd: cwd.to_owned(),
            args: args.into_iter().map(Into::into).collect(),
            stdin: None,
            timeout_ms: None,
            max_output_bytes: None,
        }
    }
}

fn trim_trailing_slashes(value: &str) -> &str {
    value.trim_end_matches('/')
}

fn repository_api_path(repository: &str) -> String {
    format!("repos/{}", repository.split('/').map(encode_uri_component).collect::<Vec<_>>().join("/"))
}

struct Inner {
    process: VcsProcess,
    http: reqwest::Client,
    environment: ForgejoEnvironment,
    clock: SharedClock,
    auth_lock: tokio::sync::Mutex<()>,
    authenticated: std::sync::Mutex<HashMap<String, (String, i64)>>,
}

/// The `ForgejoCli` service.
#[derive(Clone)]
pub struct ForgejoCli {
    inner: Arc<Inner>,
}

impl ForgejoCli {
    pub fn new(process: VcsProcess, environment: ForgejoEnvironment, clock: SharedClock) -> Self {
        Self::with_http_client(process, environment, clock, crate::http::client(None))
    }

    /// With a given HTTP client for the `fj` API path (it must not follow redirects).
    pub fn with_http_client(process: VcsProcess, environment: ForgejoEnvironment, clock: SharedClock, http: reqwest::Client) -> Self {
        Self {
            inner: Arc::new(Inner {
                process,
                http,
                environment,
                clock,
                auth_lock: tokio::sync::Mutex::new(()),
                authenticated: std::sync::Mutex::default(),
            }),
        }
    }

    /// `execute`. Failures of `fj` keep no cause (its output can hold tokens).
    pub async fn execute(&self, input: ForgejoExecuteInput) -> Result<VcsProcessOutput, ForgejoCliError> {
        let mut run = VcsProcessInput::new("ForgejoCli.execute", input.command.as_str(), input.args.iter().cloned(), input.cwd.as_str());
        run.timeout_ms = Some(input.timeout_ms.unwrap_or(30_000));
        run.stdin = input.stdin.clone();
        run.max_output_bytes = input.max_output_bytes;
        self.inner.process.run(run).await.map_err(|cause| {
            let (reason, detail) = match &cause {
                VcsProcessError::Spawn { .. } => (
                    Some(ForgejoErrorReason::MissingCli),
                    "Install Forgejo CLI (`fj` 0.6 or later) or Gitea CLI (`tea` 0.16 or later) and retry.",
                ),
                VcsProcessError::Exit {
                    failure_kind: Some(VcsProcessExitFailureKind::Authentication),
                    ..
                } => (
                    Some(ForgejoErrorReason::Authentication),
                    "Authenticate this server with `fj auth login`, `fj auth add-token`, or `tea login add`.",
                ),
                _ => (None, "Forgejo CLI command failed."),
            };
            ForgejoCliError {
                command: input.command,
                cwd: input.cwd.clone(),
                detail: detail.into(),
                reason,
                http_status: None,
                cause: (input.command != ForgejoCommand::Fj).then(|| Cause::new(cause)),
            }
        })
    }

    /// `readKeys`: the first `keys.json` that exists, or no keys.
    pub async fn read_keys(&self, cwd: &str) -> Result<ForgejoKeys, ForgejoCliError> {
        let storage_error =
            || ForgejoCliError::new(ForgejoCommand::Fj, cwd, "Could not read fj authentication storage.").with_reason(ForgejoErrorReason::Authentication);
        for path in self.inner.environment.keys_paths() {
            match tokio::fs::try_exists(&path).await {
                Ok(false) => continue,
                Ok(true) => {}
                Err(_) => return Err(storage_error()),
            }
            let raw = tokio::fs::read_to_string(&path).await.map_err(|_| storage_error())?;
            return parse_forgejo_keys(&raw).ok_or_else(|| {
                ForgejoCliError::new(ForgejoCommand::Fj, cwd, "fj authentication storage is invalid. Authenticate again with fj.")
                    .with_reason(ForgejoErrorReason::Authentication)
            });
        }
        Ok(ForgejoKeys::default())
    }

    /// `publicLogins`: `fj` keys presented as logins (no tokens).
    pub fn public_logins(keys: &ForgejoKeys, remote_url: Option<&str>) -> Vec<ForgejoLogin> {
        static HTTP: OnceLock<Regex> = OnceLock::new();
        let http = HTTP.get_or_init(|| Regex::new(r"(?i)^http://").expect("valid regex"));
        let remote = remote_url.and_then(parse_forgejo_remote);
        let mut logins = Vec::new();
        for host in keys.hosts.keys() {
            let Some(url) = parse_forgejo_remote(&format!("https://{host}")) else {
                continue;
            };
            // fj 0.6 drops URL mounts during whoami and OAuth renewal; tea supports them.
            if !url.path.is_empty() {
                continue;
            }
            // fj omits the scheme in storage. Only an explicit matching HTTP remote opts into HTTP.
            let scheme = match &remote {
                Some(remote) if !remote.ssh && remote.host == url.host && http.is_match(remote_url.unwrap_or_default()) => "http",
                _ => "https",
            };
            let login = ForgejoLogin {
                name: host.clone(),
                url: format!("{scheme}://{host}"),
                ssh_host: None,
                valid: None,
                user: String::new(),
                default: "false".into(),
            };
            let aliases: Vec<ForgejoLogin> = keys
                .aliases
                .iter()
                .filter(|(_, target)| target == host)
                .map(|(alias, _)| ForgejoLogin {
                    ssh_host: Some(alias.clone()),
                    ..login.clone()
                })
                .collect();
            if aliases.is_empty() {
                logins.push(login);
            } else {
                logins.extend(aliases);
            }
        }
        logins
    }

    /// `listLogins`.
    pub async fn list_logins(&self, cwd: &str, command: ForgejoCommand, remote_url: Option<&str>) -> Result<Vec<ForgejoLogin>, ForgejoCliError> {
        if command == ForgejoCommand::Fj {
            return match self.read_keys(cwd).await {
                Ok(keys) => Ok(Self::public_logins(&keys, remote_url)),
                Err(error) => {
                    // Stale credentials from an uninstalled fj must not disable an available tea login.
                    match self.execute(ForgejoExecuteInput::new(ForgejoCommand::Fj, cwd, ["version"])).await {
                        Err(available) if available.reason == Some(ForgejoErrorReason::MissingCli) => Ok(Vec::new()),
                        _ => Err(error),
                    }
                }
            };
        }
        let output = self
            .execute(ForgejoExecuteInput::new(ForgejoCommand::Tea, cwd, ["login", "list", "--output", "json"]))
            .await?;
        Ok(parse_forgejo_logins(&output.stdout))
    }

    /// `requestFj`: one API call with an `fj` token. Redirects are not followed; the URL must stay
    /// below `<baseUrl>/api/v1/`.
    async fn request_fj(
        &self,
        cwd: &str,
        base_url: &str,
        token: &str,
        path: &str,
        method: Option<&str>,
        body: Option<String>,
    ) -> Result<VcsProcessOutput, ForgejoCliError> {
        let failed = || ForgejoCliError::new(ForgejoCommand::Fj, cwd, "Forgejo API request failed or timed out.");
        let base = parse_url(&format!("{base_url}/api/v1/")).ok_or_else(failed)?;
        let url = base.join(path).map_err(|_| failed())?;
        if url_origin(&url) != url_origin(&base) || !url.path().starts_with(base.path()) || !url.username().is_empty() || url.password().is_some() {
            return Err(ForgejoCliError::new(ForgejoCommand::Fj, cwd, "Invalid Forgejo API path."));
        }
        let method = reqwest::Method::from_bytes(method.unwrap_or("GET").as_bytes()).map_err(|_| failed())?;
        let mut request = self.inner.http.request(method, url.as_str()).header("Authorization", format!("token {token}"));
        if let Some(body) = body {
            request = request.header("content-type", "application/json").body(body);
        }
        let send = async {
            let response = request.send().await.map_err(|_| failed())?;
            let status = response.status().as_u16();
            let link = response.headers().get("link").and_then(|v| v.to_str().ok()).map(str::to_owned);
            let body = if status == 204 || status == 205 {
                crate::http::CollectedText::default()
            } else {
                collect_body(response, 8 * 1024 * 1024).await.map_err(|_| failed())?
            };
            Ok::<_, ForgejoCliError>((status, link, body))
        };
        let (status, link, body) = tokio::time::timeout(Duration::from_secs(30), send).await.map_err(|_| failed())??;
        if body.truncated || body.invalid_utf8 {
            return Err(
                ForgejoCliError::new(ForgejoCommand::Fj, cwd, "Forgejo returned an oversized or invalid response.")
                    .with_reason(ForgejoErrorReason::InvalidResponse),
            );
        }
        if !(200..300).contains(&status) {
            let detail = if status == 404 {
                "Forgejo repository or pull request was not found.".to_owned()
            } else if !body.text.is_empty() {
                format!("Forgejo API request failed (HTTP {status}): {}", body.text)
            } else {
                format!("Forgejo API request failed (HTTP {status}). Check this server's fj credentials and permissions.")
            };
            return Err(ForgejoCliError {
                command: ForgejoCommand::Fj,
                cwd: cwd.to_owned(),
                detail,
                reason: ForgejoErrorReason::from_status(status),
                http_status: Some(status),
                cause: None,
            });
        }
        Ok(VcsProcessOutput {
            exit_code: 0,
            stdout: body.text,
            stderr: format!("HTTP/1.1 {status}\n{}", link.map(|l| format!("link: {l}\n")).unwrap_or_default()),
            ..VcsProcessOutput::default()
        })
    }

    /// `authenticateFj`: lets `fj whoami` refresh the token (at most every 30 s per server), then
    /// reads it back.
    async fn authenticate_fj(&self, cwd: &str, login: &ForgejoLogin) -> Result<String, ForgejoCliError> {
        let _guard = self.inner.auth_lock.lock().await;
        let keys = self.read_keys(cwd).await?;
        let token = keys.hosts.get(&login.name).cloned();
        let now = self.inner.clock.now_millis();
        let cached = self.inner.authenticated.lock().expect("auth cache lock").get(&login.url).cloned();
        if let (Some(token), Some((cached_token, time))) = (&token, &cached) {
            if cached_token == token && now - time < 30_000 {
                return Ok(token.clone());
            }
        }
        self.execute(ForgejoExecuteInput::new(ForgejoCommand::Fj, cwd, ["--host", login.url.as_str(), "whoami"]))
            .await?;
        // fj owns OAuth renewal. Re-read the file after it has refreshed an expired token.
        let refreshed = self.read_keys(cwd).await?.hosts.get(&login.name).cloned().ok_or_else(|| {
            ForgejoCliError::new(ForgejoCommand::Fj, cwd, "fj has no credentials for this server. Authenticate again with fj.")
                .with_reason(ForgejoErrorReason::Authentication)
        })?;
        self.inner
            .authenticated
            .lock()
            .expect("auth cache lock")
            .insert(login.url.clone(), (refreshed.clone(), now));
        Ok(refreshed)
    }

    /// `getAccount`: the login of the `fj` account for a server.
    pub async fn get_account(&self, cwd: &str, base_url: &str) -> Result<String, ForgejoCliError> {
        let logins = self.list_logins(cwd, ForgejoCommand::Fj, Some(base_url)).await?;
        let login = logins
            .into_iter()
            .find(|item| trim_trailing_slashes(&item.url) == trim_trailing_slashes(base_url))
            .ok_or_else(|| {
                ForgejoCliError::new(ForgejoCommand::Fj, cwd, "fj has no credentials for this server.").with_reason(ForgejoErrorReason::Authentication)
            })?;
        let token = self.authenticate_fj(cwd, &login).await?;
        let user = self.request_fj(cwd, trim_trailing_slashes(&login.url), &token, "user", None, None).await?;
        let account = serde_json::from_str::<Value>(&user.stdout)
            .ok()
            .and_then(|v| v.get("login").and_then(Value::as_str).map(str::to_owned))
            .filter(|login| !js_trim(login).is_empty());
        account.ok_or_else(|| {
            ForgejoCliError::new(ForgejoCommand::Fj, cwd, "Forgejo returned an invalid account response.").with_reason(ForgejoErrorReason::InvalidResponse)
        })
    }

    /// `resolveRepository`.
    pub async fn resolve_repository(&self, input: &ForgejoRepositoryInput) -> Result<ForgejoRepository, ForgejoCliError> {
        self.resolve_target(input, false).await
    }

    /// `resolveTarget`: which CLI, login and `owner/repo` serve a request.
    async fn resolve_target(&self, input: &ForgejoRepositoryInput, host_only: bool) -> Result<ForgejoRepository, ForgejoCliError> {
        static FETCH_LINE: OnceLock<Regex> = OnceLock::new();
        static PULLS: OnceLock<Regex> = OnceLock::new();
        static OWNER_REPO: OnceLock<Regex> = OnceLock::new();
        let cwd = input.cwd.as_str();
        let reference_remote = input.reference.as_deref().and_then(parse_forgejo_remote);
        let mut remote_url: Option<String> = [
            input.reference.as_deref(),
            input.repository.as_deref(),
            input.context.as_ref().map(|c| c.remote_url.as_str()),
        ]
        .into_iter()
        .flatten()
        .find(|value| !value.is_empty() && parse_forgejo_remote(value).is_some())
        .map(str::to_owned);
        let mut remote = reference_remote
            .clone()
            .or_else(|| input.repository.as_deref().and_then(parse_forgejo_remote))
            .or_else(|| input.context.as_ref().and_then(|c| parse_forgejo_remote(&c.remote_url)));
        let host = input.host.as_deref().filter(|h| !h.is_empty());
        if remote.is_none() && (input.repository.as_deref().is_none_or(str::is_empty) || host.is_some()) {
            let args: &[&str] = if host.is_some() {
                &["remote", "-v"]
            } else {
                &["remote", "get-url", "origin"]
            };
            let mut run = VcsProcessInput::new("ForgejoCli.remote", "git", args.iter().copied(), cwd);
            run.allow_non_zero_exit = true;
            let result = self.inner.process.run(run).await.map_err(|cause| {
                ForgejoCliError::new(ForgejoCommand::Tea, cwd, "Could not resolve the Forgejo repository remote.").with_cause(Cause::new(cause))
            })?;
            if let Some(host) = host {
                let fetch_line = FETCH_LINE.get_or_init(|| Regex::new(r"^\S+\s+(https?://\S+)\s+\(fetch\)$").expect("valid regex"));
                let mut matching: Vec<String> = Vec::new();
                for line in result.stdout.split('\n') {
                    if let Some(url) = fetch_line.captures(js_trim(line)).and_then(|c| c.get(1)).map(|m| m.as_str().to_owned()) {
                        if parse_forgejo_remote(&url).is_some_and(|r| r.host == host.to_lowercase()) && !matching.contains(&url) {
                            matching.push(url);
                        }
                    }
                }
                let mut origins: Vec<String> = Vec::new();
                for url in &matching {
                    if let Some(origin) = parse_url(url).map(|u| url_origin(&u)) {
                        if !origins.contains(&origin) {
                            origins.push(origin);
                        }
                    }
                }
                remote_url = if matching.len() == 1 {
                    matching.pop()
                } else if origins.len() == 1 {
                    origins.pop()
                } else {
                    None
                };
            } else {
                remote_url = Some(js_trim(&result.stdout).to_owned());
            }
            remote = remote_url.as_deref().filter(|u| !u.is_empty()).and_then(parse_forgejo_remote);
        }
        if let Some(host) = host {
            let lower = host.to_lowercase();
            let keep = remote.as_ref().is_some_and(|r| r.ssh || r.host == lower || r.hostname == lower);
            if !keep {
                remote = Some(ForgejoRemote {
                    host: lower,
                    hostname: host.split(':').next().unwrap_or(host).to_owned(),
                    ssh: false,
                    path: remote.as_ref().map(|r| r.path.clone()).unwrap_or_default(),
                });
            }
        }
        let scheme_remote_url = if remote.as_ref().is_some_and(|r| r.ssh) {
            input.context.as_ref().map(|c| c.provider.base_url.clone())
        } else {
            remote_url.clone()
        };
        let fj_logins = self
            .list_logins(cwd, ForgejoCommand::Fj, scheme_remote_url.as_deref().filter(|u| !u.is_empty()))
            .await?;
        let requested_host = host
            .map(str::to_owned)
            .or_else(|| input.context.as_ref().and_then(|c| c.requested_host.clone()));
        let match_host_only = host_only || (host.is_some() && remote.as_ref().is_none_or(|r| r.path.is_empty()));
        let select_login = |logins: &[ForgejoLogin]| -> Option<ForgejoLogin> {
            match &remote {
                Some(remote) => match_forgejo_login(logins, remote, if remote.ssh { requested_host.as_deref() } else { None }, match_host_only),
                None => logins.iter().find(|l| l.default == "true").cloned().or_else(|| {
                    let mut names: Vec<&str> = logins.iter().map(|l| l.name.as_str()).collect();
                    names.dedup();
                    names.sort_unstable();
                    names.dedup();
                    (names.len() == 1).then(|| logins[0].clone())
                }),
            }
        };
        let mut login = select_login(&fj_logins);
        let mut command = ForgejoCommand::Fj;
        if login.is_none()
            && fj_logins.iter().any(|item| match &remote {
                None => true,
                Some(remote) => match_forgejo_login(
                    std::slice::from_ref(item),
                    remote,
                    if remote.ssh { requested_host.as_deref() } else { None },
                    match_host_only,
                )
                .is_some(),
            })
        {
            match self.execute(ForgejoExecuteInput::new(ForgejoCommand::Fj, cwd, ["version"])).await {
                Ok(_) => {
                    return Err(ForgejoCliError::new(
                        ForgejoCommand::Fj,
                        cwd,
                        "Multiple fj logins match this repository. Specify its full server URL.",
                    )
                    .with_reason(ForgejoErrorReason::Authentication))
                }
                Err(error) if error.reason != Some(ForgejoErrorReason::MissingCli) => return Err(error),
                Err(_) => {}
            }
        }
        if let Some(current) = &login {
            if let Err(error) = self.authenticate_fj(cwd, current).await {
                if error.reason == Some(ForgejoErrorReason::MissingCli) {
                    login = None;
                } else {
                    return Err(error);
                }
            }
        }
        if login.is_none() {
            command = ForgejoCommand::Tea;
            login = select_login(&self.list_logins(cwd, ForgejoCommand::Tea, None).await?);
        }
        let Some(mut login) = login else {
            return Err(ForgejoCliError::new(
                ForgejoCommand::Tea,
                cwd,
                "No matching Forgejo login. Use `fj auth login`, `fj auth add-token`, or `tea login add` for this server; choose a default when multiple tea accounts match.",
            )
            .with_reason(ForgejoErrorReason::Authentication));
        };
        if host_only {
            return Ok(ForgejoRepository {
                command,
                login: login.name.clone(),
                repository: String::new(),
                base_url: trim_trailing_slashes(&login.url).to_owned(),
            });
        }
        let path = match &reference_remote {
            Some(reference) => reference.path.clone(),
            None => match input.repository.as_deref().filter(|r| !r.is_empty() && parse_forgejo_remote(r).is_none()) {
                Some(repository) => repository.to_owned(),
                None => remote.as_ref().map(|r| r.path.clone()).unwrap_or_default(),
            },
        };
        let base_path = parse_url(&login.url).map(|u| u.path().trim_matches('/').to_owned()).unwrap_or_default();
        let relative_path = if !base_path.is_empty() && path.split('/').count() > 2 && path.starts_with(&format!("{base_path}/")) {
            path[base_path.len() + 1..].to_owned()
        } else {
            path
        };
        let pulls = PULLS.get_or_init(|| Regex::new(r"/pulls/[0-9]+.*$").expect("valid regex"));
        let without_pulls = pulls.replace(&relative_path, "").into_owned();
        let repository_path = without_pulls.strip_suffix(".git").unwrap_or(&without_pulls).to_owned();
        if command == ForgejoCommand::Fj && !repository_path.contains('/') {
            login.user = self.get_account(cwd, &login.url).await?;
        }
        let repository = if repository_path.contains('/') {
            repository_path
        } else {
            format!("{}/{repository_path}", login.user)
        };
        if !OWNER_REPO
            .get_or_init(|| Regex::new(r"^[^/\s]+/[^/\s]+$").expect("valid regex"))
            .is_match(&repository)
        {
            return Err(ForgejoCliError::new(
                command,
                cwd,
                "Specify a Forgejo repository as owner/repository or its full server URL.",
            ));
        }
        Ok(ForgejoRepository {
            command,
            login: login.name.clone(),
            repository,
            base_url: trim_trailing_slashes(&login.url).to_owned(),
        })
    }

    /// `api`: one API call through `fj`'s token or `tea api`.
    pub async fn api(&self, input: &ForgejoApiInput) -> Result<VcsProcessOutput, ForgejoCliError> {
        static STATUS: OnceLock<Regex> = OnceLock::new();
        let cwd = input.target.cwd.as_str();
        let method = input.method.as_deref().unwrap_or("GET");
        let host_only = input.path.trim_start_matches('/') == "user" && method == "GET";
        let repository = self.resolve_target(&input.target, host_only).await?;
        let stdin = input.body.as_ref().map(Value::to_string);
        let mut path = input.path.trim_start_matches('/').to_owned();
        if let Some(requested) = input.target.repository.as_deref().filter(|r| !r.is_empty()) {
            if requested != repository.repository {
                // Repository identities retain the server mount path; API routes do not.
                let prefix = repository_api_path(requested);
                if path == prefix || path.starts_with(&format!("{prefix}/")) || path.starts_with(&format!("{prefix}?")) {
                    path = format!("{}{}", repository_api_path(&repository.repository), &path[prefix.len()..]);
                }
            }
        }
        if repository.command == ForgejoCommand::Fj {
            let token = self.read_keys(cwd).await?.hosts.get(&repository.login).cloned().ok_or_else(|| {
                ForgejoCliError::new(ForgejoCommand::Fj, cwd, "fj has no credentials for this server.").with_reason(ForgejoErrorReason::Authentication)
            })?;
            return self.request_fj(cwd, &repository.base_url, &token, &path, input.method.as_deref(), stdin).await;
        }
        let mut args = vec!["api".to_owned(), "--include".into(), "--login".into(), repository.login.clone()];
        if !repository.repository.is_empty() {
            args.extend(["--repo".to_owned(), repository.repository.clone()]);
        }
        args.extend(["--method".to_owned(), method.to_owned()]);
        if input.body.is_some() {
            args.extend(["--data".to_owned(), "@-".into()]);
        }
        args.push(format!("{}/api/v1/{path}", repository.base_url));
        let mut execute = ForgejoExecuteInput::new(ForgejoCommand::Tea, cwd, args);
        execute.stdin = stdin;
        let result = self.execute(execute).await?;
        // tea reports HTTP failures with exit code zero; use its response status.
        let status: Option<u16> = STATUS
            .get_or_init(|| Regex::new(r"(?m)^HTTP/\S+ ([0-9]{3})").expect("valid regex"))
            .captures(&result.stderr)
            .and_then(|c| c.get(1)?.as_str().parse().ok())
            .filter(|s| *s != 0);
        match status {
            Some(status) if status < 400 => Ok(result),
            _ => {
                let detail = match status {
                    Some(401 | 403) => "Forgejo denied access. Check this server's `tea login` credentials and permissions.".to_owned(),
                    Some(404) => "Forgejo repository or pull request was not found.".to_owned(),
                    Some(429) => "Forgejo API rate limit exceeded.".to_owned(),
                    Some(status) => format!("Forgejo API request failed (HTTP {status})."),
                    None => "Forgejo API request failed without an HTTP status.".to_owned(),
                };
                Err(ForgejoCliError {
                    command: ForgejoCommand::Tea,
                    cwd: cwd.to_owned(),
                    detail,
                    reason: status.and_then(ForgejoErrorReason::from_status),
                    http_status: status,
                    cause: None,
                })
            }
        }
    }
}

/// `repos/<owner>/<repo>` with each segment encoded.
pub fn forgejo_repository_path(repository: &str) -> String {
    repository_api_path(repository)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn login(name: &str, url: &str, ssh_host: Option<&str>, default: &str) -> ForgejoLogin {
        ForgejoLogin {
            name: name.into(),
            url: url.into(),
            ssh_host: ssh_host.map(Into::into),
            valid: None,
            user: "maria".into(),
            default: default.into(),
        }
    }

    // SourceControlDiscovery.test.ts: "does not choose a default Forgejo login across ambiguous SSH server ports"
    #[test]
    fn does_not_choose_a_default_login_across_ambiguous_ssh_ports() {
        let logins = vec![
            login("one", "http://forgejo.local:3000", Some("forgejo.local"), "true"),
            login("two", "http://forgejo.local:4000", Some("forgejo.local"), "false"),
        ];
        let remote = parse_forgejo_remote("git@forgejo.local:maria/project.git").unwrap();
        assert_eq!(parse_forgejo_remote("forgejo.local:maria/project.git").unwrap(), remote);
        assert!(match_forgejo_login(&logins, &remote, None, false).is_none());
        assert_eq!(match_forgejo_login(&logins, &remote, Some("forgejo.local:4000"), false).unwrap().name, "two");
        assert!(match_forgejo_login(&logins, &remote, Some("other.local:4000"), false).is_none());
        let alias = parse_forgejo_remote("git@ssh.forgejo.local:maria/project.git").unwrap();
        assert!(match_forgejo_login(&logins, &alias, Some("forgejo.local:4000"), false).is_none());
        let https = parse_forgejo_remote("http://forgejo.local:4000/maria/project.git").unwrap();
        assert_eq!(match_forgejo_login(&logins, &https, None, false).unwrap().name, "two");
        let host_only = parse_forgejo_remote("http://forgejo.local:4000").unwrap();
        assert_eq!(match_forgejo_login(&logins, &host_only, None, true).unwrap().name, "two");
        let mounted: Vec<ForgejoLogin> = logins
            .iter()
            .map(|l| ForgejoLogin {
                url: format!("http://forgejo.local:4000/{}", l.name),
                ..l.clone()
            })
            .collect();
        assert!(match_forgejo_login(&mounted, &host_only, None, true).is_none());
    }

    #[test]
    fn keys_paths_follow_fj_project_dirs() {
        let env = ForgejoEnvironment {
            platform: "linux".into(),
            home: "/home/maria".into(),
            data_home: Some("relative".into()),
            app_data: None,
        };
        assert_eq!(env.keys_paths(), vec![PathBuf::from("/home/maria/.local/share/forgejo-cli/keys.json")]);
        let mac = ForgejoEnvironment {
            platform: "darwin".into(),
            ..env
        };
        assert_eq!(
            mac.keys_paths()[1],
            PathBuf::from("/home/maria/Library/Application Support/Cyborus.forgejo-cli/keys.json")
        );
    }
}
