//! `BitbucketApi.ts`: Bitbucket Cloud over its REST API (`api.bitbucket.org/2.0`).
//!
//! - Credentials come from the server settings (`bitbucket.accessToken`, or `email` +
//!   `apiToken`), read on every request so a saved token applies without a restart, then from
//!   `T3CODE_BITBUCKET_ACCESS_TOKEN` / `T3CODE_BITBUCKET_EMAIL` + `T3CODE_BITBUCKET_API_TOKEN`.
//!   A value that is not visible ASCII is treated as unset (it could not travel in a header).
//! - Credentials only ever go to the configured API origin: paged `next` URLs and redirect
//!   targets elsewhere are refused ([`BitbucketApiError::UntrustedUrl`]). Redirects are followed
//!   here (at most 3), never by the HTTP client.
//! - `Retry-After` on an error answer becomes `retry_at`.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use base64::Engine;
use regex::Regex;
use serde_json::{json, Value};
use zc_contracts::{
    BitbucketSettings, EOption, SourceControlProviderAuth, SourceControlProviderAuthStatus as Auth, SourceControlProviderKind,
    SourceControlRepositoryCloneUrls, SourceControlRepositoryVisibility,
};
use zc_vcs::contracts::VcsSwitchRefInput;
use zc_vcs::{GitVcsDriver, VcsDriverRegistry};

use crate::bitbucket::pull_requests::*;
use crate::errors::{error_defect, Cause, CauseError};
use crate::github::cli::SchemaDecodeError;
use crate::http::collect_body;
use crate::provider::{parse_source_control_owner_ref, source_branch, ChangeRequestStateFilter, SourceControlProviderContext, SourceControlRefSelector};
use crate::rate_limit::retry_at_from_header;
use crate::records::NormalizedChangeRequest;
use crate::util::{encode_uri_component, js_length, js_trim, parse_url, url_origin, SharedClock};

pub const DEFAULT_API_BASE_URL: &str = "https://api.bitbucket.org/2.0";
/// A response body past this is cut short, so one huge diff cannot exhaust the server.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_REDIRECTS: u32 = 3;
const NO_CREDENTIAL_DETAIL: &str = "Add a Bitbucket token in Settings → Source Control, or set the T3CODE_BITBUCKET_* environment variables on the server.";

/// `BitbucketApiEnvConfig`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitbucketApiConfig {
    pub base_url: String,
    pub access_token: Option<String>,
    pub email: Option<String>,
    pub api_token: Option<String>,
}

impl Default for BitbucketApiConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_API_BASE_URL.into(),
            access_token: None,
            email: None,
            api_token: None,
        }
    }
}

impl BitbucketApiConfig {
    /// `T3CODE_BITBUCKET_API_BASE_URL`, `…_ACCESS_TOKEN`, `…_EMAIL`, `…_API_TOKEN`.
    pub fn from_env() -> Self {
        let var = |key: &str| std::env::var(key).ok();
        Self {
            base_url: var("T3CODE_BITBUCKET_API_BASE_URL").unwrap_or_else(|| DEFAULT_API_BASE_URL.into()),
            access_token: var("T3CODE_BITBUCKET_ACCESS_TOKEN"),
            email: var("T3CODE_BITBUCKET_EMAIL"),
            api_token: var("T3CODE_BITBUCKET_API_TOKEN"),
        }
    }
}

/// Where the saved Bitbucket credentials come from (`ServerSettingsService.getSettings`).
#[async_trait]
pub trait BitbucketCredentialSource: Send + Sync {
    /// The `bitbucket` section of the server settings, unredacted.
    async fn bitbucket_settings(&self) -> Result<BitbucketSettings, String>;
}

/// Fixed settings (tests, or a server without a settings service).
#[derive(Debug, Clone, Default)]
pub struct StaticBitbucketSettings(pub Option<BitbucketSettings>);

#[async_trait]
impl BitbucketCredentialSource for StaticBitbucketSettings {
    async fn bitbucket_settings(&self) -> Result<BitbucketSettings, String> {
        Ok(self.0.clone().unwrap_or_else(empty_settings))
    }
}

/// Reads `bitbucket` from a [`zc_ports::SettingsService`].
pub struct SettingsPortCredentials(pub Arc<dyn zc_ports::SettingsService>);

#[async_trait]
impl BitbucketCredentialSource for SettingsPortCredentials {
    async fn bitbucket_settings(&self) -> Result<BitbucketSettings, String> {
        let settings = self.0.get_settings().await.map_err(|error| format!("{error:?}"))?;
        match serde_json::to_value(&settings).unwrap_or_default().get("bitbucket") {
            None | Some(Value::Null) => Ok(empty_settings()),
            Some(section) => serde_json::from_value(section.clone()).map_err(|error| error.to_string()),
        }
    }
}

fn empty_settings() -> BitbucketSettings {
    serde_json::from_value(json!({"email": "", "accessToken": "", "apiToken": ""})).expect("default Bitbucket settings")
}

/// `BitbucketApiOperation`.
pub type BitbucketApiOperation = &'static str;

/// `BitbucketApiError`.
#[derive(Debug, Clone)]
pub enum BitbucketApiError {
    UntrustedUrl {
        host: String,
    },
    RepositoryLocator {
        repository: String,
    },
    Request {
        operation: BitbucketApiOperation,
        cause: Cause,
    },
    Response {
        operation: BitbucketApiOperation,
        status: u16,
        response_body_length: usize,
        retry_at: Option<i64>,
    },
    ResponseBodyRead {
        operation: BitbucketApiOperation,
        status: u16,
        retry_at: Option<i64>,
        cause: Cause,
    },
    ResponseDecode {
        operation: BitbucketApiOperation,
        status: u16,
        cause: Cause,
    },
    RepositoryVcsResolve {
        cwd: String,
        cause: Cause,
    },
    RepositoryRemotesList {
        cwd: String,
        cause: Cause,
    },
    RepositoryRemoteNotFound {
        cwd: String,
    },
    PullRequestBodyRead {
        cwd: String,
        body_file: String,
        cause: Cause,
    },
    Checkout {
        cwd: String,
        reference: String,
        cause: Cause,
    },
}

impl BitbucketApiError {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::UntrustedUrl { .. } => "BitbucketUntrustedUrlError",
            Self::RepositoryLocator { .. } => "BitbucketRepositoryLocatorError",
            Self::Request { .. } => "BitbucketRequestError",
            Self::Response { .. } => "BitbucketResponseError",
            Self::ResponseBodyRead { .. } => "BitbucketResponseBodyReadError",
            Self::ResponseDecode { .. } => "BitbucketResponseDecodeError",
            Self::RepositoryVcsResolve { .. } => "BitbucketRepositoryVcsResolveError",
            Self::RepositoryRemotesList { .. } => "BitbucketRepositoryRemotesListError",
            Self::RepositoryRemoteNotFound { .. } => "BitbucketRepositoryRemoteNotFoundError",
            Self::PullRequestBodyRead { .. } => "BitbucketPullRequestBodyReadError",
            Self::Checkout { .. } => "BitbucketCheckoutError",
        }
    }

    pub fn detail(&self) -> String {
        match self {
            Self::UntrustedUrl { host } => format!("The response pointed at {host}, outside the configured Bitbucket."),
            Self::RepositoryLocator { .. } => "Bitbucket repositories must be specified as workspace/repository.".into(),
            Self::Request { .. } => "Failed to send the Bitbucket request.".into(),
            Self::Response { status, .. } | Self::ResponseBodyRead { status, .. } => format!("Bitbucket returned HTTP {status}."),
            Self::ResponseDecode { .. } => "Bitbucket returned invalid JSON for the requested resource.".into(),
            Self::RepositoryVcsResolve { cwd, .. } => format!("Failed to resolve VCS repository for {cwd}."),
            Self::RepositoryRemotesList { cwd, .. } => format!("Failed to list remotes for {cwd}."),
            Self::RepositoryRemoteNotFound { cwd } => format!("No Bitbucket repository remote was detected for {cwd}."),
            Self::PullRequestBodyRead { body_file, .. } => format!("Failed to read pull request body file {body_file}."),
            Self::Checkout { .. } => "Failed to check out the Bitbucket pull request.".into(),
        }
    }

    pub fn operation(&self) -> &'static str {
        match self {
            Self::UntrustedUrl { .. } => "request",
            Self::RepositoryLocator { .. } => "createRepository",
            Self::Request { operation, .. }
            | Self::Response { operation, .. }
            | Self::ResponseBodyRead { operation, .. }
            | Self::ResponseDecode { operation, .. } => operation,
            Self::RepositoryVcsResolve { .. } | Self::RepositoryRemotesList { .. } | Self::RepositoryRemoteNotFound { .. } => "resolveRepository",
            Self::PullRequestBodyRead { .. } => "createPullRequest",
            Self::Checkout { .. } => "checkoutPullRequest",
        }
    }

    pub fn message(&self) -> String {
        format!("Bitbucket API failed in {}: {}", self.operation(), self.detail())
    }

    /// The `Retry-After` time of a rate-limited answer.
    pub fn retry_at(&self) -> Option<i64> {
        match self {
            Self::Response { retry_at, .. } | Self::ResponseBodyRead { retry_at, .. } => *retry_at,
            _ => None,
        }
    }

    /// The HTTP status of an error answer.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Response { status, .. } | Self::ResponseBodyRead { status, .. } | Self::ResponseDecode { status, .. } => Some(*status),
            _ => None,
        }
    }

    fn cause(&self) -> Option<&Cause> {
        match self {
            Self::Request { cause, .. }
            | Self::ResponseBodyRead { cause, .. }
            | Self::ResponseDecode { cause, .. }
            | Self::RepositoryVcsResolve { cause, .. }
            | Self::RepositoryRemotesList { cause, .. }
            | Self::PullRequestBodyRead { cause, .. }
            | Self::Checkout { cause, .. } => Some(cause),
            _ => None,
        }
    }

    /// The tagged wire encoding.
    pub fn to_json(&self) -> Value {
        let fields = match self {
            Self::UntrustedUrl { host } => json!({"host": host}),
            Self::RepositoryLocator { repository } => json!({"repository": repository}),
            Self::Request { operation, .. } => json!({"operation": operation}),
            Self::Response {
                operation,
                status,
                response_body_length,
                retry_at,
            } => json!({"operation": operation, "status": status, "responseBodyLength": response_body_length, "retryAt": retry_at}),
            Self::ResponseBodyRead {
                operation, status, retry_at, ..
            } => json!({"operation": operation, "status": status, "retryAt": retry_at}),
            Self::ResponseDecode { operation, status, .. } => json!({"operation": operation, "status": status}),
            Self::RepositoryVcsResolve { cwd, .. } | Self::RepositoryRemotesList { cwd, .. } | Self::RepositoryRemoteNotFound { cwd } => json!({"cwd": cwd}),
            Self::PullRequestBodyRead { cwd, body_file, .. } => json!({"cwd": cwd, "bodyFile": body_file}),
            Self::Checkout { cwd, reference, .. } => json!({"cwd": cwd, "reference": reference}),
        };
        crate::errors::tagged(self.tag(), fields, self.cause())
    }
}

impl std::fmt::Display for BitbucketApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for BitbucketApiError {}

impl serde::Serialize for BitbucketApiError {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_json().serialize(serializer)
    }
}

impl CauseError for BitbucketApiError {
    fn defect(&self) -> Value {
        error_defect(self.tag(), self.message(), self.cause().map(Cause::defect))
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// `{workspace, repoSlug}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitbucketRepositoryLocator {
    pub workspace: String,
    pub repo_slug: String,
}

/// `parseBitbucketRepositorySlug`: the last two path segments.
pub fn parse_bitbucket_repository_slug(value: &str) -> Option<BitbucketRepositoryLocator> {
    let trimmed = js_trim(value);
    let normalized = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let parts: Vec<&str> = normalized.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return None;
    }
    Some(BitbucketRepositoryLocator {
        workspace: parts[parts.len() - 2].to_owned(),
        repo_slug: parts[parts.len() - 1].to_owned(),
    })
}

/// `parseBitbucketRemoteUrl`.
pub fn parse_bitbucket_remote_url(remote_url: &str) -> Option<BitbucketRepositoryLocator> {
    static SCP: OnceLock<Regex> = OnceLock::new();
    let trimmed = js_trim(remote_url);
    if let Some(path) = SCP
        .get_or_init(|| Regex::new(r"^[a-zA-Z0-9._-]+@[^:/]+:(.+)$").expect("valid regex"))
        .captures(trimmed)
        .and_then(|c| c.get(1))
    {
        return parse_bitbucket_repository_slug(path.as_str());
    }
    parse_url(trimmed).and_then(|url| parse_bitbucket_repository_slug(url.path()))
}

/// `normalizeChangeRequestId`.
fn normalize_change_request_id(reference: &str) -> String {
    static URL: OnceLock<Regex> = OnceLock::new();
    let trimmed = js_trim(reference);
    let trimmed = trimmed.strip_prefix('#').unwrap_or(trimmed);
    URL.get_or_init(|| Regex::new(r"(?i)(?:pull-requests|pullrequests|pull-request|pull|pr)/([0-9]+)(?:[^0-9].*)?$").expect("valid regex"))
        .captures(trimmed)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_owned()))
        .unwrap_or_else(|| trimmed.to_owned())
}

fn to_bitbucket_states(state: ChangeRequestStateFilter) -> &'static [&'static str] {
    match state {
        ChangeRequestStateFilter::Open => &["OPEN"],
        ChangeRequestStateFilter::Closed => &["DECLINED", "SUPERSEDED"],
        ChangeRequestStateFilter::Merged => &["MERGED"],
        ChangeRequestStateFilter::All => &["OPEN", "MERGED", "DECLINED", "SUPERSEDED"],
    }
}

fn state_filter(states: &[&str]) -> String {
    if states.len() == 1 {
        format!("state = \"{}\"", states[0])
    } else {
        format!("({})", states.iter().map(|s| format!("state = \"{s}\"")).collect::<Vec<_>>().join(" OR "))
    }
}

/// `sanitizeBranchFragment` (`shared/git.ts`).
pub fn sanitize_branch_fragment(raw: &str) -> String {
    static QUOTES: OnceLock<Regex> = OnceLock::new();
    static EDGES: OnceLock<Regex> = OnceLock::new();
    static INVALID: OnceLock<Regex> = OnceLock::new();
    static SLASHES: OnceLock<Regex> = OnceLock::new();
    static DASHES: OnceLock<Regex> = OnceLock::new();
    static EDGES2: OnceLock<Regex> = OnceLock::new();
    static TRAILING: OnceLock<Regex> = OnceLock::new();
    let re = |cell: &'static OnceLock<Regex>, pattern: &str| cell.get_or_init(|| Regex::new(pattern).expect("valid regex"));
    let lowered = js_trim(raw).to_lowercase();
    let normalized = re(&QUOTES, r#"['"`]"#).replace_all(&lowered, "");
    let normalized = re(&EDGES, r"^[./\s_-]+|[./\s_-]+$").replace_all(&normalized, "");
    let fragment = re(&INVALID, r"[^a-z0-9/_-]+").replace_all(&normalized, "-");
    let fragment = re(&SLASHES, r"/+").replace_all(&fragment, "/");
    let fragment = re(&DASHES, r"-+").replace_all(&fragment, "-");
    let fragment = re(&EDGES2, r"^[./_-]+|[./_-]+$").replace_all(&fragment, "").into_owned();
    let sliced: String = fragment.chars().take(64).collect();
    let fragment = re(&TRAILING, r"[./_-]+$").replace_all(&sliced, "").into_owned();
    if fragment.is_empty() {
        "update".into()
    } else {
        fragment
    }
}

#[derive(Debug, Clone)]
struct RawRepository {
    full_name: String,
    html: Option<String>,
    clone: Vec<(String, String)>,
    main_branch: Option<String>,
}

fn decode_raw_repository(value: &Value) -> Option<RawRepository> {
    let map = value.as_object()?;
    let full_name = crate::util::trimmed_non_empty(map.get("full_name")?)?;
    let links = map.get("links")?.as_object()?;
    let html = match links.get("html") {
        None => None,
        Some(html) => Some(crate::util::trimmed_non_empty(html.as_object()?.get("href")?)?),
    };
    let clone = match links.get("clone") {
        None => Vec::new(),
        Some(clone) => clone
            .as_array()?
            .iter()
            .map(|entry| {
                let entry = entry.as_object()?;
                Some((
                    crate::util::trimmed_non_empty(entry.get("name")?)?,
                    crate::util::trimmed_non_empty(entry.get("href")?)?,
                ))
            })
            .collect::<Option<Vec<_>>>()?,
    };
    let main_branch = match map.get("mainbranch") {
        None | Some(Value::Null) => None,
        Some(Value::Object(branch)) => Some(crate::util::trimmed_non_empty(branch.get("name")?)?),
        Some(_) => return None,
    };
    Some(RawRepository {
        full_name,
        html,
        clone,
        main_branch,
    })
}

fn normalize_repository_clone_urls(raw: &RawRepository) -> SourceControlRepositoryCloneUrls {
    let named = |name: &str| raw.clone.iter().find(|(n, _)| n.to_lowercase() == name).map(|(_, href)| href.clone());
    let http_clone = named("https").or_else(|| raw.html.clone());
    let ssh_clone = named("ssh");
    SourceControlRepositoryCloneUrls {
        name_with_owner: raw.full_name.clone(),
        url: http_clone.clone().or_else(|| raw.html.clone()).unwrap_or_else(|| raw.full_name.clone()),
        ssh_url: ssh_clone.or(http_clone).unwrap_or_else(|| raw.full_name.clone()),
    }
}

/// `RawBitbucketBranchingModelSchema` → the development branch settings.
#[derive(Debug, Clone, Default)]
struct BranchingModel {
    development: Option<Development>,
}

#[derive(Debug, Clone, Default)]
struct Development {
    branch_name: Option<String>,
    is_valid: Option<bool>,
    name: Option<String>,
    use_mainbranch: Option<bool>,
}

fn decode_branching_model(value: &Value) -> Option<BranchingModel> {
    let map = value.as_object()?;
    let development = match map.get("development") {
        None => None,
        Some(Value::Object(dev)) => {
            let branch_name = match dev.get("branch") {
                None | Some(Value::Null) => None,
                Some(Value::Object(branch)) => match branch.get("name") {
                    None => None,
                    Some(name) => Some(crate::util::trimmed_non_empty(name)?),
                },
                Some(_) => return None,
            };
            Some(Development {
                branch_name,
                is_valid: crate::util::optional_bool(dev, "is_valid").ok()?,
                name: crate::util::optional_string(dev, "name", true).ok()?,
                use_mainbranch: crate::util::optional_bool(dev, "use_mainbranch").ok()?,
            })
        }
        Some(_) => return None,
    };
    Some(BranchingModel { development })
}

fn default_change_request_target_branch(repository: &RawRepository, model: Option<&BranchingModel>) -> Option<String> {
    let main = repository.main_branch.clone();
    let Some(development) = model.and_then(|m| m.development.as_ref()) else {
        return main;
    };
    if development.use_mainbranch == Some(true) || development.is_valid == Some(false) {
        return main;
    }
    let branch = development
        .branch_name
        .as_deref()
        .map(|b| js_trim(b).to_owned())
        .or_else(|| development.name.as_deref().map(|n| js_trim(n).to_owned()))
        .unwrap_or_default();
    if branch.is_empty() || branch == "null" {
        main
    } else {
        Some(branch)
    }
}

#[derive(Debug, Clone)]
enum Credential {
    AccessToken(String),
    ApiToken { email: String, api_token: String },
}

fn header_safe(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

fn credential_from(access_token: &str, email: &str, api_token: &str) -> Option<Credential> {
    if header_safe(access_token) {
        return Some(Credential::AccessToken(access_token.to_owned()));
    }
    if header_safe(email) && header_safe(api_token) {
        return Some(Credential::ApiToken {
            email: email.to_owned(),
            api_token: api_token.to_owned(),
        });
    }
    None
}

fn resolve_credential(settings: &BitbucketSettings, env: &BitbucketApiConfig) -> Option<Credential> {
    credential_from(&settings.access_token, &settings.email, &settings.api_token).or_else(|| {
        credential_from(
            env.access_token.as_deref().unwrap_or_default(),
            env.email.as_deref().unwrap_or_default(),
            env.api_token.as_deref().unwrap_or_default(),
        )
    })
}

fn auth_from_credential(credential: Option<&Credential>) -> SourceControlProviderAuth {
    let host = EOption::some("bitbucket.org".to_owned());
    match credential {
        Some(Credential::AccessToken(_)) => SourceControlProviderAuth {
            status: Auth::Unknown,
            account: EOption::none(),
            host,
            detail: EOption::some("An access token is configured.".into()),
        },
        Some(Credential::ApiToken { email, .. }) => SourceControlProviderAuth {
            status: Auth::Unknown,
            account: EOption::some(email.clone()),
            host,
            detail: EOption::some("An API token is configured.".into()),
        },
        None => SourceControlProviderAuth {
            status: Auth::Unauthenticated,
            account: EOption::none(),
            host,
            detail: EOption::some(NO_CREDENTIAL_DETAIL.into()),
        },
    }
}

fn origin_of(value: &str) -> Option<String> {
    parse_url(value).map(|url| url_origin(&url))
}

/// An HTTP failure kept as a cause.
#[derive(Debug, Clone)]
pub struct HttpClientError(pub String);

impl CauseError for HttpClientError {
    fn defect(&self) -> Value {
        error_defect("HttpClientError", self.0.clone(), None)
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// `request` input.
#[derive(Debug, Clone)]
pub struct BitbucketRequest {
    /// `GET`, `POST`, `PUT` or `DELETE`.
    pub method: String,
    /// A path below the API base, or a whole URL on the configured Bitbucket.
    pub url: String,
    /// A JSON document.
    pub body: Option<String>,
    /// Response bytes to keep.
    pub max_bytes: Option<usize>,
}

/// `request` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitbucketResponseBody {
    pub body: String,
    pub truncated: bool,
}

struct Inner {
    config: BitbucketApiConfig,
    credentials: Arc<dyn BitbucketCredentialSource>,
    http: reqwest::Client,
    clock: SharedClock,
    git: GitVcsDriver,
    vcs: VcsDriverRegistry,
}

/// The `BitbucketApi` service.
#[derive(Clone)]
pub struct BitbucketApi {
    inner: Arc<Inner>,
}

type ApiResult<T> = Result<T, BitbucketApiError>;

impl BitbucketApi {
    pub fn new(
        config: BitbucketApiConfig,
        credentials: Arc<dyn BitbucketCredentialSource>,
        clock: SharedClock,
        git: GitVcsDriver,
        vcs: VcsDriverRegistry,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                credentials,
                http: crate::http::client(None),
                clock,
                git,
                vcs,
            }),
        }
    }

    fn api_url(&self, path: &str) -> String {
        format!("{}{path}", self.inner.config.base_url.trim_end_matches('/'))
    }

    /// Read on every request so credentials saved in settings apply without a restart.
    async fn current_credential(&self) -> Option<Credential> {
        match self.inner.credentials.bitbucket_settings().await {
            Ok(settings) => resolve_credential(&settings, &self.inner.config),
            Err(_) => {
                // No cause: a settings decode error can quote a hand-edited token.
                tracing::warn!("failed to read Bitbucket credentials from settings");
                resolve_credential(&empty_settings(), &self.inner.config)
            }
        }
    }

    async fn with_auth(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.current_credential().await {
            None => request,
            Some(Credential::AccessToken(token)) => request.header("authorization", format!("Bearer {token}")),
            Some(Credential::ApiToken { email, api_token }) => {
                let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{email}:{api_token}"));
                request.header("authorization", format!("Basic {encoded}"))
            }
        }
    }

    async fn response_error(&self, operation: BitbucketApiOperation, response: reqwest::Response) -> BitbucketApiError {
        let status = response.status().as_u16();
        let now = self.inner.clock.now_millis();
        let retry_at = retry_at_from_header(response.headers().get("retry-after").and_then(|v| v.to_str().ok()), now);
        match collect_body(response, DEFAULT_MAX_RESPONSE_BYTES).await {
            Ok(collected) => BitbucketApiError::Response {
                operation,
                status,
                response_body_length: js_length(&collected.text),
                retry_at,
            },
            Err(error) => BitbucketApiError::ResponseBodyRead {
                operation,
                status,
                retry_at,
                cause: Cause::new(HttpClientError(error.to_string())),
            },
        }
    }

    /// `executeJson`: an authenticated JSON request decoded with `decode`.
    async fn execute_json<T>(&self, operation: BitbucketApiOperation, request: reqwest::RequestBuilder, decode: impl Fn(&Value) -> Option<T>) -> ApiResult<T> {
        let request = self.with_auth(request.header("accept", "application/json")).await;
        let response = request.send().await.map_err(|error| BitbucketApiError::Request {
            operation,
            cause: Cause::new(HttpClientError(error.to_string())),
        })?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(self.response_error(operation, response).await);
        }
        let decode_error = |message: String| BitbucketApiError::ResponseDecode {
            operation,
            status,
            cause: Cause::new(SchemaDecodeError(message)),
        };
        let text = response.text().await.map_err(|e| decode_error(e.to_string()))?;
        let value: Value = serde_json::from_str(&text).map_err(|e| decode_error(e.to_string()))?;
        decode(&value).ok_or_else(|| decode_error("The response does not match the schema".into()))
    }

    fn get(&self, url: &str) -> reqwest::RequestBuilder {
        self.inner.http.get(url)
    }

    /// `resolveRepository`: the explicit repository, the context's Bitbucket remote, or the
    /// first Bitbucket remote of the checkout.
    pub async fn resolve_repository(
        &self,
        cwd: &str,
        context: Option<&SourceControlProviderContext>,
        repository: Option<&str>,
    ) -> ApiResult<BitbucketRepositoryLocator> {
        if let Some(locator) = repository.and_then(parse_bitbucket_repository_slug) {
            return Ok(locator);
        }
        if let Some(context) = context.filter(|c| c.provider.kind == SourceControlProviderKind::Bitbucket) {
            if let Some(locator) = parse_bitbucket_remote_url(&context.remote_url) {
                return Ok(locator);
            }
        }
        let handle = self
            .inner
            .vcs
            .resolve(cwd, None)
            .await
            .map_err(|cause| BitbucketApiError::RepositoryVcsResolve {
                cwd: cwd.to_owned(),
                cause: Cause::new(cause),
            })?;
        let remotes = handle
            .driver
            .list_remotes(cwd)
            .await
            .map_err(|cause| BitbucketApiError::RepositoryRemotesList {
                cwd: cwd.to_owned(),
                cause: Cause::new(cause),
            })?;
        for remote in remotes.remotes {
            let is_bitbucket = zc_vcs::shared_git::detect_source_control_provider_from_remote_url(&remote.url)
                .is_some_and(|p| p.kind == zc_vcs::contracts::SourceControlProviderKind::Bitbucket);
            if !is_bitbucket {
                continue;
            }
            if let Some(locator) = parse_bitbucket_remote_url(&remote.url) {
                return Ok(locator);
            }
        }
        Err(BitbucketApiError::RepositoryRemoteNotFound { cwd: cwd.to_owned() })
    }

    fn repository_url(&self, repository: &BitbucketRepositoryLocator, suffix: &str) -> String {
        self.api_url(&format!(
            "/repositories/{}/{}{suffix}",
            encode_uri_component(&repository.workspace),
            encode_uri_component(&repository.repo_slug)
        ))
    }

    async fn get_repository_from_locator(&self, repository: &BitbucketRepositoryLocator) -> ApiResult<RawRepository> {
        self.execute_json("getRepository", self.get(&self.repository_url(repository, "")), decode_raw_repository)
            .await
    }

    async fn get_raw_pull_request_from_repository(&self, repository: &BitbucketRepositoryLocator, reference: &str) -> ApiResult<BitbucketPullRequest> {
        let url = self.repository_url(
            repository,
            &format!("/pullrequests/{}", encode_uri_component(&normalize_change_request_id(reference))),
        );
        self.execute_json("getPullRequest", self.get(&url), decode_bitbucket_pull_request).await
    }

    /// `probeAuth`: `GET /user`, else what the configured credential says.
    pub async fn probe_auth(&self) -> SourceControlProviderAuth {
        let decode_user = |value: &Value| -> Option<Option<String>> {
            let map = value.as_object()?;
            let field = |key: &str| -> Result<Option<String>, ()> {
                match map.get(key) {
                    None => Ok(None),
                    Some(value) => crate::util::trimmed_non_empty(value).map(Some).ok_or(()),
                }
            };
            let username = field("username").ok()?;
            let display_name = field("display_name").ok()?;
            let account_id = field("account_id").ok()?;
            Some(username.or(display_name).or(account_id))
        };
        match self.execute_json("probeAuth", self.get(&self.api_url("/user")), decode_user).await {
            Ok(account) => SourceControlProviderAuth {
                status: Auth::Authenticated,
                account: EOption(account.map(|a| js_trim(&a).to_owned()).filter(|a| !a.is_empty())),
                host: EOption::some("bitbucket.org".into()),
                detail: EOption::none(),
            },
            Err(_) => auth_from_credential(self.current_credential().await.as_ref()),
        }
    }

    /// The one origin these credentials may be sent to.
    fn trusted_url(&self, value: &str) -> Option<String> {
        static HTTP: OnceLock<Regex> = OnceLock::new();
        if !HTTP.get_or_init(|| Regex::new(r"^https?://").expect("valid regex")).is_match(value) {
            return Some(self.api_url(value));
        }
        let origin = origin_of(value)?;
        (Some(origin) == origin_of(&self.inner.config.base_url)).then(|| value.to_owned())
    }

    /// `send`: follows redirects itself, only back to the configured Bitbucket.
    async fn send(&self, method: &str, url: &str, body: Option<&str>) -> ApiResult<reqwest::Response> {
        let mut current = url.to_owned();
        let mut redirects = 0;
        loop {
            let Some(target) = self.trusted_url(&current) else {
                return Err(BitbucketApiError::UntrustedUrl {
                    host: origin_of(&current).unwrap_or_else(|| "an unreadable url".into()),
                });
            };
            let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
            // No `Accept: application/json`: the diff endpoints answer with a patch, not JSON.
            let mut request = self.inner.http.request(method, target.as_str());
            if let Some(body) = body {
                request = request.header("content-type", "application/json").body(body.to_owned());
            }
            let response = self.with_auth(request).await.send().await.map_err(|error| BitbucketApiError::Request {
                operation: "request",
                cause: Cause::new(HttpClientError(error.to_string())),
            })?;
            let status = response.status().as_u16();
            let location = response.headers().get("location").and_then(|v| v.to_str().ok()).map(str::to_owned);
            match location {
                Some(location) if (300..400).contains(&status) && redirects < MAX_REDIRECTS => {
                    let base = parse_url(&target).ok_or_else(|| BitbucketApiError::UntrustedUrl {
                        host: "an unreadable url".into(),
                    })?;
                    current = base.join(&location).map(|u| u.to_string()).map_err(|_| BitbucketApiError::UntrustedUrl {
                        host: "an unreadable url".into(),
                    })?;
                    redirects += 1;
                }
                _ => return Ok(response),
            }
        }
    }

    /// `request`: one authenticated request, body returned as text (bounded).
    pub async fn request(&self, input: BitbucketRequest) -> ApiResult<BitbucketResponseBody> {
        let response = self.send(&input.method, &input.url, input.body.as_deref()).await?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(self.response_error("request", response).await);
        }
        let collected = collect_body(response, input.max_bytes.unwrap_or(DEFAULT_MAX_RESPONSE_BYTES))
            .await
            .map_err(|error| BitbucketApiError::ResponseBodyRead {
                operation: "request",
                status,
                retry_at: None,
                cause: Cause::new(HttpClientError(error.to_string())),
            })?;
        Ok(BitbucketResponseBody {
            body: collected.text,
            truncated: collected.truncated,
        })
    }

    /// `listPullRequests`.
    pub async fn list_pull_requests(
        &self,
        cwd: &str,
        context: Option<&SourceControlProviderContext>,
        head_selector: &str,
        source: Option<&SourceControlRefSelector>,
        state: ChangeRequestStateFilter,
        limit: Option<u32>,
    ) -> ApiResult<Vec<NormalizedChangeRequest>> {
        let repository = self.resolve_repository(cwd, context, None).await?;
        let states = to_bitbucket_states(state);
        let mut url = url::Url::parse(&self.repository_url(&repository, "/pullrequests")).map_err(|e| BitbucketApiError::Request {
            operation: "listPullRequests",
            cause: Cause::message(e.to_string()),
        })?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("pagelen", &limit.unwrap_or(20).clamp(1, 50).to_string());
            query.append_pair("sort", "-updated_on");
            query.append_pair(
                "q",
                &format!(
                    "source.branch.name = \"{}\" AND {}",
                    source_branch(head_selector, source).replace('"', "\\\""),
                    state_filter(states)
                ),
            );
            for state in states {
                query.append_pair("state", state);
            }
        }
        let list = self
            .execute_json("listPullRequests", self.get(url.as_str()), decode_bitbucket_pull_request_list)
            .await?;
        Ok(list.iter().map(normalize_bitbucket_pull_request).collect())
    }

    /// `getPullRequest`.
    pub async fn get_pull_request(&self, cwd: &str, context: Option<&SourceControlProviderContext>, reference: &str) -> ApiResult<NormalizedChangeRequest> {
        let repository = self.resolve_repository(cwd, context, None).await?;
        let raw = self.get_raw_pull_request_from_repository(&repository, reference).await?;
        Ok(normalize_bitbucket_pull_request(&raw))
    }

    /// `getRepositoryCloneUrls`.
    pub async fn get_repository_clone_urls(
        &self,
        cwd: &str,
        context: Option<&SourceControlProviderContext>,
        repository: &str,
    ) -> ApiResult<SourceControlRepositoryCloneUrls> {
        let locator = self.resolve_repository(cwd, context, Some(repository)).await?;
        Ok(normalize_repository_clone_urls(&self.get_repository_from_locator(&locator).await?))
    }

    /// `createRepository`.
    pub async fn create_repository(&self, repository: &str, visibility: SourceControlRepositoryVisibility) -> ApiResult<SourceControlRepositoryCloneUrls> {
        let locator = parse_bitbucket_repository_slug(repository).ok_or_else(|| BitbucketApiError::RepositoryLocator {
            repository: repository.to_owned(),
        })?;
        let body = json!({"scm": "git", "is_private": visibility == SourceControlRepositoryVisibility::Private});
        let request = self
            .inner
            .http
            .post(self.repository_url(&locator, ""))
            .header("content-type", "application/json")
            .body(body.to_string());
        let raw = self.execute_json("createRepository", request, decode_raw_repository).await?;
        Ok(normalize_repository_clone_urls(&raw))
    }

    /// `createPullRequest`: the description is read from the body file.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_pull_request(
        &self,
        cwd: &str,
        context: Option<&SourceControlProviderContext>,
        base_branch: &str,
        head_selector: &str,
        source: Option<&SourceControlRefSelector>,
        target: Option<&SourceControlRefSelector>,
        title: &str,
        body_file: &str,
    ) -> ApiResult<()> {
        let repository = self.resolve_repository(cwd, context, None).await?;
        let description = tokio::fs::read_to_string(body_file)
            .await
            .map_err(|cause| BitbucketApiError::PullRequestBodyRead {
                cwd: cwd.to_owned(),
                body_file: body_file.to_owned(),
                cause: Cause::new(cause),
            })?;
        let source_owner = source
            .and_then(|s| s.owner.clone())
            .or_else(|| parse_source_control_owner_ref(head_selector).and_then(|s| s.owner));
        let mut source_json = json!({"branch": {"name": source_branch(head_selector, source)}});
        if let Some(owner) = source_owner {
            let repository_name = source.and_then(|s| s.repository.clone()).unwrap_or_else(|| repository.repo_slug.clone());
            source_json["repository"] = json!({"full_name": format!("{owner}/{repository_name}")});
        }
        let body = json!({
            "title": title,
            "description": description,
            "source": source_json,
            "destination": {"branch": {"name": target.map_or(base_branch, |t| t.ref_name.as_str())}},
        });
        let request = self
            .inner
            .http
            .post(self.repository_url(&repository, "/pullrequests"))
            .header("content-type", "application/json")
            .body(body.to_string());
        self.execute_json("createPullRequest", request, decode_bitbucket_pull_request).await.map(drop)
    }

    /// `getDefaultBranch`: the branching model's development branch, else the main branch.
    pub async fn get_default_branch(&self, cwd: &str, context: Option<&SourceControlProviderContext>) -> ApiResult<Option<String>> {
        let locator = self.resolve_repository(cwd, context, None).await?;
        let model_url = self.repository_url(&locator, "/branching-model");
        let (repository, model) = futures::join!(
            self.get_repository_from_locator(&locator),
            self.execute_json("getBranchingModel", self.get(&model_url), decode_branching_model)
        );
        Ok(default_change_request_target_branch(&repository?, model.ok().as_ref()))
    }

    async fn resolve_checkout_remote(
        &self,
        cwd: &str,
        context: Option<&SourceControlProviderContext>,
        destination: &BitbucketRepositoryLocator,
        source_repository_name: &str,
        is_cross_repository: bool,
    ) -> Result<String, CheckoutFailure> {
        if let Some(context) = context {
            if context.provider.kind == SourceControlProviderKind::Bitbucket
                && !is_cross_repository
                && parse_bitbucket_remote_url(&context.remote_url).is_some()
            {
                return Ok(context.remote_name.clone());
            }
        }
        if !is_cross_repository {
            if let Ok(name) = self.inner.git.resolve_primary_remote_name(cwd).await {
                if !name.is_empty() {
                    return Ok(name);
                }
            }
        }
        let locator = self.resolve_repository(cwd, context, Some(source_repository_name)).await?;
        let clone_urls = normalize_repository_clone_urls(&self.get_repository_from_locator(&locator).await?);
        let origin = self.inner.git.read_config_value(cwd, "remote.origin.url").await.ok().flatten();
        let url = if origin.as_deref().is_some_and(zc_vcs::shared_git::is_ssh_remote_url) {
            clone_urls.ssh_url
        } else {
            clone_urls.url
        };
        let preferred = if is_cross_repository {
            source_repository_name
                .split('/')
                .next()
                .map(js_trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("bitbucket")
                .to_owned()
        } else {
            destination.workspace.clone()
        };
        self.inner.git.ensure_remote(cwd, &preferred, &url).await.map_err(CheckoutFailure::git)
    }

    /// `checkoutPullRequest`: fetches the PR branch (through a fork remote for cross-repository
    /// PRs), sets its upstream and switches to it. A git-specific escape hatch: Bitbucket has no
    /// checkout CLI.
    pub async fn checkout_pull_request(&self, cwd: &str, context: Option<&SourceControlProviderContext>, reference: &str, force: bool) -> ApiResult<()> {
        let result: Result<(), CheckoutFailure> = async {
            let destination = self.resolve_repository(cwd, context, None).await?;
            let pull = self.get_raw_pull_request_from_repository(&destination, reference).await?;
            let name_of = |repository: &Option<BitbucketRepositoryRef>| {
                repository
                    .as_ref()
                    .and_then(|r| r.full_name.as_deref())
                    .map(js_trim)
                    .filter(|n| !n.is_empty())
                    .map(str::to_owned)
            };
            let destination_name = name_of(&pull.destination.repository).unwrap_or_else(|| format!("{}/{}", destination.workspace, destination.repo_slug));
            let source_name = name_of(&pull.source.repository).unwrap_or_else(|| destination_name.clone());
            let is_cross = source_name != destination_name;
            let remote = self.resolve_checkout_remote(cwd, context, &destination, &source_name, is_cross).await?;
            let remote_branch = pull.source.branch_name.clone();
            let local_branch = if is_cross {
                format!("t3code/pr-{}/{}", pull.id, sanitize_branch_fragment(&remote_branch))
            } else {
                remote_branch.clone()
            };
            let local_exists = self
                .inner
                .git
                .list_local_branch_names(cwd)
                .await
                .map_err(CheckoutFailure::git)?
                .contains(&local_branch);
            if force || !local_exists {
                self.inner
                    .git
                    .fetch_remote_branch(cwd, &remote, &remote_branch, &local_branch)
                    .await
                    .map_err(CheckoutFailure::git)?;
            } else {
                self.inner
                    .git
                    .fetch_remote_tracking_branch(cwd, &remote, &remote_branch)
                    .await
                    .map_err(CheckoutFailure::git)?;
            }
            self.inner
                .git
                .set_branch_upstream(cwd, &local_branch, &remote, &remote_branch)
                .await
                .map_err(CheckoutFailure::git)?;
            self.inner
                .git
                .switch_ref(&VcsSwitchRefInput {
                    cwd: cwd.to_owned(),
                    ref_name: local_branch,
                })
                .await
                .map_err(CheckoutFailure::git)?;
            Ok(())
        }
        .await;
        result.map_err(|failure| match failure {
            CheckoutFailure::Api(error) => error,
            CheckoutFailure::Other(cause) => BitbucketApiError::Checkout {
                cwd: cwd.to_owned(),
                reference: reference.to_owned(),
                cause,
            },
        })
    }
}

enum CheckoutFailure {
    Api(BitbucketApiError),
    Other(Cause),
}

impl CheckoutFailure {
    fn git(error: zc_vcs::GitCommandError) -> Self {
        Self::Other(Cause::new(error))
    }
}

impl From<BitbucketApiError> for CheckoutFailure {
    fn from(error: BitbucketApiError) -> Self {
        Self::Api(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_branch_fragments_like_shared_git() {
        assert_eq!(sanitize_branch_fragment("  Feature/My Branch!! "), "feature/my-branch");
        assert_eq!(sanitize_branch_fragment("..."), "update");
        assert_eq!(sanitize_branch_fragment("it's \"quoted\""), "its-quoted");
    }

    #[test]
    fn parses_remote_urls() {
        let scp = parse_bitbucket_remote_url("git@bitbucket.org:team/project.git").unwrap();
        assert_eq!((scp.workspace.as_str(), scp.repo_slug.as_str()), ("team", "project"));
        let https = parse_bitbucket_remote_url("https://user@bitbucket.org/team/project.git").unwrap();
        assert_eq!(https.repo_slug, "project");
        assert!(parse_bitbucket_remote_url("project").is_none());
        assert_eq!(normalize_change_request_id("https://bitbucket.org/team/project/pull-requests/42/diff"), "42");
    }
}
