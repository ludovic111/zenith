//! `GitHubCli.ts`: every `gh` invocation of the server.
//!
//! - [`GitHubCli::execute`] is the shared entry point (the PR service uses it for `gh api`
//!   GraphQL calls too). Reads that cost GraphQL points (`pr list`, `pr view`, `repo view`) are
//!   gated: a per-host pause after a rate-limit answer ([`SourceControlRateLimit`]), a quota
//!   probe (`gh api rate_limit`, cached 30 s per host and credential) and the GraphQL budget with
//!   its 10% reserve ([`GitHubGraphQlBudget`]). Interactive reads pass `allow_reserve`.
//! - A pinned credential ([`with_pinned_github_credential`], the TS `PinnedGitHubCredential`
//!   reference) runs `gh` with `GH_HOST`/`GH_TOKEN`/… set, and only for commands that provably
//!   target that host.
//! - Failures are classified like `fromVcsError`: missing `gh`, authentication, rate limit,
//!   not found, other.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use regex::Regex;
use serde::{Serialize, Serializer};
use serde_json::json;
use zc_contracts::{SourceControlProviderKind, SourceControlRepositoryCloneUrls, SourceControlRepositoryVisibility};
use zc_core::vcs_process::{VcsProcess, VcsProcessError, VcsProcessExitFailureKind, VcsProcessInput, VcsProcessOutput};

use crate::cache::ClockedCache;
use crate::errors::{error_defect, tagged, Cause, CauseError};
use crate::github::pull_requests::{decode_github_pull_request_json, decode_github_pull_request_list_json};
use crate::graphql_budget::GitHubGraphQlBudget;
use crate::rate_limit::{current_credential_scope, RateLimitKey, SourceControlRateLimit};
use crate::records::NormalizedChangeRequest;
use crate::util::{js_trim, parse_url, trimmed_non_empty, url_host, url_origin, SharedClock};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const PR_LIST_FIELDS: &str =
    "number,title,url,baseRefName,headRefName,state,isDraft,mergedAt,closedAt,isCrossRepository,headRepository,headRepositoryOwner";
pub const PR_VIEW_FIELDS: &str =
    "number,title,url,baseRefName,headRefName,state,isDraft,mergedAt,closedAt,updatedAt,isCrossRepository,headRepository,headRepositoryOwner";

/// `PinnedGitHubCredential`: a server-local credential scope. Never put its token in RPC payloads
/// or cache keys.
#[derive(Clone, PartialEq, Eq)]
pub struct PinnedGitHubCredential {
    pub host: String,
    pub token: String,
    pub credential_fingerprint: String,
}

impl std::fmt::Debug for PinnedGitHubCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedGitHubCredential")
            .field("host", &self.host)
            .field("credential_fingerprint", &self.credential_fingerprint)
            .finish_non_exhaustive()
    }
}

tokio::task_local! {
    static PINNED_GITHUB_CREDENTIAL: Option<PinnedGitHubCredential>;
    static ALLOW_GITHUB_RESERVE: bool;
}

/// Runs `future` with `gh` pinned to `credential` (`Effect.provideService(PinnedGitHubCredential, …)`).
pub async fn with_pinned_github_credential<F: std::future::Future>(credential: PinnedGitHubCredential, future: F) -> F::Output {
    PINNED_GITHUB_CREDENTIAL.scope(Some(credential), future).await
}

/// Runs `future` with `AllowGitHubReserve` set: its reads may spend the reserved quota.
pub async fn with_github_reserve<F: std::future::Future>(future: F) -> F::Output {
    ALLOW_GITHUB_RESERVE.scope(true, future).await
}

fn pinned_credential() -> Option<PinnedGitHubCredential> {
    PINNED_GITHUB_CREDENTIAL.try_with(Clone::clone).ok().flatten()
}

fn allow_reserve_in_scope() -> bool {
    ALLOW_GITHUB_RESERVE.try_with(|allow| *allow).unwrap_or(false)
}

/// The `PinnedGitHubCredential` of the current task (`yield* PinnedGitHubCredential`).
pub fn current_pinned_github_credential() -> Option<PinnedGitHubCredential> {
    pinned_credential()
}

/// The `AllowGitHubReserve` of the current task (`yield* AllowGitHubReserve`).
pub fn github_reserve_allowed() -> bool {
    allow_reserve_in_scope()
}

fn repository_host(repository: Option<&str>) -> Option<String> {
    static HTTP: OnceLock<Regex> = OnceLock::new();
    let repository = repository?;
    if HTTP.get_or_init(|| Regex::new(r"(?i)^https?://").expect("valid regex")).is_match(repository) {
        return parse_url(repository).map(|url| url_host(&url).to_lowercase());
    }
    let parts: Vec<&str> = repository.split('/').collect();
    (parts.len() == 3).then(|| parts[0].to_lowercase())
}

/// `commandHosts`: the hosts a `gh` command line targets (`None` for a target with no host).
pub fn command_hosts(args: &[String]) -> Vec<Option<String>> {
    static HTTP: OnceLock<Regex> = OnceLock::new();
    let http = HTTP.get_or_init(|| Regex::new(r"(?i)^https?://").expect("valid regex"));
    let mut hosts = Vec::new();
    if args.first().map(String::as_str) == Some("repo") && args.get(1).map(String::as_str) == Some("view") {
        hosts.push(repository_host(args.get(2).map(String::as_str)));
    }
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--hostname" {
            index += 1;
            hosts.push(args.get(index).map(|h| h.to_lowercase()));
        } else if let Some(host) = arg.strip_prefix("--hostname=") {
            hosts.push(Some(host.to_lowercase()));
        } else if arg == "--repo" || arg == "-R" {
            index += 1;
            hosts.push(repository_host(args.get(index).map(String::as_str)));
        } else if let Some(repository) = arg.strip_prefix("--repo=") {
            hosts.push(repository_host(Some(repository)));
        } else if let Some(repository) = arg.strip_prefix("-R") {
            hosts.push(repository_host(Some(repository)));
        } else if http.is_match(arg) {
            hosts.push(repository_host(Some(arg)));
        }
        index += 1;
    }
    hosts
}

fn targets_verified_host(args: &[String], host: &str) -> bool {
    let hosts = command_hosts(args);
    !hosts.is_empty() && hosts.iter().all(|target| target.as_deref() == Some(host))
}

/// The members of the `GitHubCliError` union.
#[derive(Debug, Clone, PartialEq)]
pub enum GitHubCliErrorKind {
    Unavailable,
    Authentication,
    RateLimit { retry_at: Option<i64> },
    PullRequestNotFound,
    Command,
    PullRequestListDecode,
    ChangeRequestListDecode,
    PullRequestDecode,
    RepositoryDecode,
}

/// `GitHubCliError`.
#[derive(Debug, Clone)]
pub struct GitHubCliError {
    pub kind: GitHubCliErrorKind,
    pub cwd: String,
    pub cause: Cause,
}

impl GitHubCliError {
    pub fn new(kind: GitHubCliErrorKind, cwd: impl Into<String>, cause: Cause) -> Self {
        Self { kind, cwd: cwd.into(), cause }
    }

    /// Always `"gh"`.
    pub fn command(&self) -> &'static str {
        "gh"
    }

    pub fn tag(&self) -> &'static str {
        match self.kind {
            GitHubCliErrorKind::Unavailable => "GitHubCliUnavailableError",
            GitHubCliErrorKind::Authentication => "GitHubCliAuthenticationError",
            GitHubCliErrorKind::RateLimit { .. } => "GitHubCliRateLimitError",
            GitHubCliErrorKind::PullRequestNotFound => "GitHubPullRequestNotFoundError",
            GitHubCliErrorKind::Command => "GitHubCliCommandError",
            GitHubCliErrorKind::PullRequestListDecode => "GitHubPullRequestListDecodeError",
            GitHubCliErrorKind::ChangeRequestListDecode => "GitHubChangeRequestListDecodeError",
            GitHubCliErrorKind::PullRequestDecode => "GitHubPullRequestDecodeError",
            GitHubCliErrorKind::RepositoryDecode => "GitHubRepositoryDecodeError",
        }
    }

    pub fn detail(&self) -> &'static str {
        match self.kind {
            GitHubCliErrorKind::Unavailable => "GitHub CLI (`gh`) is required but not available on PATH.",
            GitHubCliErrorKind::Authentication => "GitHub CLI is not authenticated. Run `gh auth login` and retry.",
            GitHubCliErrorKind::RateLimit { .. } => "GitHub API rate limit exceeded. Run `gh api rate_limit` to inspect the quota and reset time.",
            GitHubCliErrorKind::PullRequestNotFound => "Pull request not found. Check the PR number or URL and try again.",
            GitHubCliErrorKind::Command => "GitHub CLI command failed.",
            GitHubCliErrorKind::PullRequestListDecode => "GitHub CLI returned invalid PR list JSON.",
            GitHubCliErrorKind::ChangeRequestListDecode => "GitHub CLI returned invalid change request JSON.",
            GitHubCliErrorKind::PullRequestDecode => "GitHub CLI returned invalid pull request JSON.",
            GitHubCliErrorKind::RepositoryDecode => "GitHub CLI returned invalid repository JSON.",
        }
    }

    pub fn message(&self) -> String {
        let operation = match self.kind {
            GitHubCliErrorKind::PullRequestListDecode => "listOpenPullRequests",
            GitHubCliErrorKind::ChangeRequestListDecode => "listChangeRequests",
            GitHubCliErrorKind::PullRequestDecode => "getPullRequest",
            GitHubCliErrorKind::RepositoryDecode => "getRepositoryCloneUrls",
            _ => "execute",
        };
        format!("GitHub CLI failed in {operation}: {}", self.detail())
    }

    pub fn is_rate_limit(&self) -> bool {
        matches!(self.kind, GitHubCliErrorKind::RateLimit { .. })
    }

    /// `fromVcsError({command: "gh", cwd}, error)`.
    pub fn from_vcs_error(cwd: &str, error: VcsProcessError) -> Self {
        let kind = match &error {
            VcsProcessError::Spawn { cause, .. } if is_missing_executable(cwd, &cause.message()) => GitHubCliErrorKind::Unavailable,
            VcsProcessError::Exit {
                failure_kind: Some(VcsProcessExitFailureKind::Authentication),
                ..
            } => GitHubCliErrorKind::Authentication,
            VcsProcessError::Exit {
                failure_kind: Some(VcsProcessExitFailureKind::RateLimited),
                ..
            } => GitHubCliErrorKind::RateLimit { retry_at: None },
            VcsProcessError::Exit {
                failure_kind: Some(VcsProcessExitFailureKind::NotFound),
                ..
            } => GitHubCliErrorKind::PullRequestNotFound,
            _ => GitHubCliErrorKind::Command,
        };
        Self::new(kind, cwd, Cause::new(error))
    }
}

/// A spawn failure means the executable is missing when the OS said "not found" while the
/// working directory exists (TS reads this from the `PlatformError` reason and module).
pub(crate) fn is_missing_executable(cwd: &str, message: &str) -> bool {
    let lower = message.to_lowercase();
    (lower.contains("no such file or directory") || lower.contains("not found") || lower.contains("os error 2")) && std::path::Path::new(cwd).is_dir()
}

impl std::fmt::Display for GitHubCliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for GitHubCliError {}

impl Serialize for GitHubCliError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let retry_at = match self.kind {
            GitHubCliErrorKind::RateLimit { retry_at } => retry_at,
            _ => None,
        };
        tagged(self.tag(), json!({"command": "gh", "cwd": self.cwd, "retryAt": retry_at}), Some(&self.cause)).serialize(serializer)
    }
}

impl CauseError for GitHubCliError {
    fn defect(&self) -> serde_json::Value {
        error_defect(self.tag(), self.message(), Some(self.cause.defect()))
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// A schema decode failure kept as a cause.
#[derive(Debug, Clone)]
pub struct SchemaDecodeError(pub String);

impl CauseError for SchemaDecodeError {
    fn defect(&self) -> serde_json::Value {
        error_defect("SchemaError", self.0.clone(), None)
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// `execute` input.
#[derive(Debug, Clone, Default)]
pub struct GitHubExecuteInput {
    pub cwd: String,
    pub args: Vec<String>,
    pub timeout_ms: Option<u64>,
    /// Piped to the child's stdin, for payloads that must never appear in argv.
    pub stdin: Option<String>,
    pub env: Option<BTreeMap<String, String>>,
    pub max_output_bytes: Option<usize>,
    pub rate_limit_host: Option<String>,
    pub allow_reserve: Option<bool>,
}

impl GitHubExecuteInput {
    pub fn new<I, S>(cwd: impl Into<String>, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            cwd: cwd.into(),
            args: args.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }
}

/// `deriveRepositoryCloneUrlsFromCreateOutput`: `gh repo create` prints the new repository URL.
pub fn derive_repository_clone_urls_from_create_output(stdout: &str, repository: &str) -> SourceControlRepositoryCloneUrls {
    static URL: OnceLock<Regex> = OnceLock::new();
    let pattern = URL.get_or_init(|| Regex::new(r"https?://[^\s]+").expect("valid regex"));
    if let Some(found) = pattern.find(stdout) {
        let cleaned = found.as_str().strip_suffix(".git").unwrap_or(found.as_str());
        if let Some(parsed) = parse_url(cleaned) {
            let segments: Vec<&str> = parsed.path().trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
            if segments.len() == 2 {
                let name_with_owner = format!("{}/{}", segments[0], segments[1]);
                return SourceControlRepositoryCloneUrls {
                    url: format!("{}/{name_with_owner}", url_origin(&parsed)),
                    ssh_url: format!("git@{}:{name_with_owner}.git", url_host(&parsed)),
                    name_with_owner,
                };
            }
        }
    }
    SourceControlRepositoryCloneUrls {
        name_with_owner: repository.to_owned(),
        url: format!("https://github.com/{repository}"),
        ssh_url: format!("git@github.com:{repository}.git"),
    }
}

/// `RawGitHubRepositoryCloneUrlsSchema` from JSON.
pub fn decode_repository_clone_urls(raw: &str) -> Result<SourceControlRepositoryCloneUrls, String> {
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    let field = |key: &str| value.get(key).and_then(trimmed_non_empty).ok_or_else(|| format!("Missing or invalid {key}"));
    Ok(SourceControlRepositoryCloneUrls {
        name_with_owner: field("nameWithOwner")?,
        url: field("url")?,
        ssh_url: field("sshUrl")?,
    })
}

struct Inner {
    process: VcsProcess,
    budget: GitHubGraphQlBudget,
    limits: SourceControlRateLimit,
    quota: ClockedCache<String, (), GitHubCliError>,
}

/// The `GitHubCli` service.
#[derive(Clone)]
pub struct GitHubCli {
    inner: Arc<Inner>,
}

impl GitHubCli {
    /// `GitHubCli.make` with its own budget and rate limits.
    pub fn new(process: VcsProcess, clock: SharedClock) -> Self {
        Self::with_limits(
            process,
            GitHubGraphQlBudget::new(clock.clone()),
            SourceControlRateLimit::new(clock.clone()),
            clock,
        )
    }

    /// `GitHubCli.make` sharing the server's budget and rate limits (the PR service reads them too).
    pub fn with_limits(process: VcsProcess, budget: GitHubGraphQlBudget, limits: SourceControlRateLimit, clock: SharedClock) -> Self {
        Self {
            inner: Arc::new(Inner {
                process,
                budget,
                limits,
                quota: ClockedCache::new(32, clock, |result: &Result<(), GitHubCliError>| if result.is_ok() { 30_000 } else { 0 }),
            }),
        }
    }

    pub fn budget(&self) -> &GitHubGraphQlBudget {
        &self.inner.budget
    }

    pub fn limits(&self) -> &SourceControlRateLimit {
        &self.inner.limits
    }

    /// `executeRaw`: one `gh` process, pinned to `credential` when given.
    async fn execute_raw(
        process: &VcsProcess,
        input: &GitHubExecuteInput,
        credential: Option<&PinnedGitHubCredential>,
    ) -> Result<VcsProcessOutput, GitHubCliError> {
        if let Some(credential) = credential {
            if !targets_verified_host(&input.args, &credential.host) {
                return Err(GitHubCliError::new(
                    GitHubCliErrorKind::Command,
                    &input.cwd,
                    Cause::message("The GitHub command does not target the verified credential's host."),
                ));
            }
        }
        let mut env: Option<BTreeMap<String, Option<String>>> = input.env.as_ref().map(|env| env.iter().map(|(k, v)| (k.clone(), Some(v.clone()))).collect());
        if let Some(credential) = credential {
            let env = env.get_or_insert_with(BTreeMap::new);
            env.insert("GH_HOST".into(), Some(credential.host.clone()));
            for key in ["GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"] {
                env.insert(key.into(), Some(credential.token.clone()));
            }
            env.insert("GH_DEBUG".into(), Some(String::new()));
        }
        let mut run = VcsProcessInput::new("GitHubCli.execute", "gh", input.args.iter().cloned(), input.cwd.as_str());
        run.timeout_ms = Some(input.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        run.stdin = input.stdin.clone();
        run.env = env;
        run.max_output_bytes = input.max_output_bytes;
        process.run(run).await.map_err(|error| GitHubCliError::from_vcs_error(&input.cwd, error))
    }

    /// `execute`.
    pub async fn execute(&self, input: GitHubExecuteInput) -> Result<VcsProcessOutput, GitHubCliError> {
        let credential = pinned_credential();
        let command = input.args.first().map(String::as_str);
        let action = input.args.get(1).map(String::as_str);
        let guarded = matches!((command, action), (Some("pr"), Some("list" | "view")) | (Some("repo"), Some("view")));
        if !guarded {
            return Self::execute_raw(&self.inner.process, &input, credential.as_ref()).await;
        }
        if let Some(credential) = &credential {
            if !targets_verified_host(&input.args, &credential.host) {
                return Self::execute_raw(&self.inner.process, &input, Some(credential)).await;
            }
        }
        let allow_reserve = input.allow_reserve.unwrap_or_else(allow_reserve_in_scope);
        let host = credential
            .as_ref()
            .map(|c| c.host.clone())
            .or_else(|| command_hosts(&input.args).into_iter().flatten().next())
            .or_else(|| input.rate_limit_host.clone())
            .or_else(|| input.env.as_ref().and_then(|env| env.get("GH_HOST").cloned()))
            .or_else(|| std::env::var("GH_HOST").ok())
            .unwrap_or_else(|| "github.com".into())
            .to_lowercase();
        let key = RateLimitKey::new(SourceControlProviderKind::Github, host.clone());
        let scope = credential
            .as_ref()
            .map(|c| c.credential_fingerprint.clone())
            .unwrap_or_else(current_credential_scope);
        let paused = |cause: crate::rate_limit::SourceControlRateLimitPausedError| {
            GitHubCliError::new(
                GitHubCliErrorKind::RateLimit {
                    retry_at: Some(cause.retry_at),
                },
                &input.cwd,
                Cause::new(cause),
            )
        };
        let lease = self.inner.limits.check_in(&scope, &key, allow_reserve).map_err(paused)?;

        // Errors carry whether they count as a rate-limit answer: a pause from the budget does not.
        let counted = |error: GitHubCliError| {
            let rate_limited = error.is_rate_limit();
            (error, rate_limited)
        };
        let result: Result<VcsProcessOutput, (GitHubCliError, bool)> = async {
            let quota_key = format!("{host}\0{}", credential.as_ref().map(|c| c.credential_fingerprint.as_str()).unwrap_or(""));
            let process = self.inner.process.clone();
            let budget = self.inner.budget.clone();
            let quota_credential = credential.clone();
            let quota_host = host.clone();
            let quota_scope = scope.clone();
            self.inner
                .quota
                .get(quota_key, move || async move {
                    let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "/".into());
                    let probe = GitHubExecuteInput::new(
                        cwd,
                        [
                            "api",
                            "rate_limit",
                            "--hostname",
                            quota_host.as_str(),
                            "--jq",
                            ".resources.graphql | {data:{rateLimit:{cost:1,limit:.limit,remaining:.remaining,resetAt:(.reset|todateiso8601)}}}",
                        ],
                    );
                    let output = Self::execute_raw(&process, &probe, quota_credential.as_ref()).await?;
                    budget.observe_in(&quota_scope, &quota_host, &output.stdout);
                    Ok(())
                })
                .await
                .map_err(counted)?;
            self.inner
                .budget
                .query_in(&scope, &host, "query {}", allow_reserve)
                .map_err(|cause| (paused(cause), false))?;
            Self::execute_raw(&self.inner.process, &input, credential.as_ref()).await.map_err(counted)
        }
        .await;
        match result {
            Ok(output) => {
                self.inner.limits.record_success_in(&scope, &key, lease);
                Ok(output)
            }
            Err((error, rate_limited)) => {
                if rate_limited {
                    self.inner.limits.record_rate_limit_in(&scope, &key, lease, None);
                }
                Err(error)
            }
        }
    }

    /// `listOpenPullRequests`.
    pub async fn list_open_pull_requests(
        &self,
        cwd: &str,
        head_selector: &str,
        limit: Option<u32>,
        rate_limit_host: Option<String>,
    ) -> Result<Vec<NormalizedChangeRequest>, GitHubCliError> {
        let mut input = GitHubExecuteInput::new(
            cwd,
            [
                "pr".to_owned(),
                "list".into(),
                "--head".into(),
                head_selector.to_owned(),
                "--state".into(),
                "open".into(),
                "--limit".into(),
                limit.unwrap_or(1).to_string(),
                "--json".into(),
                PR_LIST_FIELDS.into(),
            ],
        );
        input.rate_limit_host = rate_limit_host;
        input.allow_reserve = Some(true);
        let output = self.execute(input).await?;
        let raw = js_trim(&output.stdout);
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        decode_github_pull_request_list_json(raw)
            .map_err(|e| GitHubCliError::new(GitHubCliErrorKind::PullRequestListDecode, cwd, Cause::new(SchemaDecodeError(e))))
    }

    /// `getPullRequest`.
    pub async fn get_pull_request(&self, cwd: &str, reference: &str, rate_limit_host: Option<String>) -> Result<NormalizedChangeRequest, GitHubCliError> {
        let mut input = GitHubExecuteInput::new(cwd, ["pr", "view", reference, "--json", PR_VIEW_FIELDS]);
        input.rate_limit_host = rate_limit_host;
        input.allow_reserve = Some(true);
        let output = self.execute(input).await?;
        decode_github_pull_request_json(js_trim(&output.stdout))
            .map_err(|e| GitHubCliError::new(GitHubCliErrorKind::PullRequestDecode, cwd, Cause::new(SchemaDecodeError(e))))
    }

    /// `getRepositoryCloneUrls`.
    pub async fn get_repository_clone_urls(&self, cwd: &str, repository: &str) -> Result<SourceControlRepositoryCloneUrls, GitHubCliError> {
        let output = self
            .execute(GitHubExecuteInput::new(cwd, ["repo", "view", repository, "--json", "nameWithOwner,url,sshUrl"]))
            .await?;
        decode_repository_clone_urls(js_trim(&output.stdout))
            .map_err(|e| GitHubCliError::new(GitHubCliErrorKind::RepositoryDecode, cwd, Cause::new(SchemaDecodeError(e))))
    }

    /// `createRepository`.
    pub async fn create_repository(
        &self,
        cwd: &str,
        repository: &str,
        visibility: SourceControlRepositoryVisibility,
    ) -> Result<SourceControlRepositoryCloneUrls, GitHubCliError> {
        let visibility = format!("--{}", visibility.as_str());
        let output = self
            .execute(GitHubExecuteInput::new(cwd, ["repo", "create", repository, visibility.as_str()]))
            .await?;
        Ok(derive_repository_clone_urls_from_create_output(&output.stdout, repository))
    }

    /// `createPullRequest`.
    pub async fn create_pull_request(&self, cwd: &str, base_branch: &str, head_selector: &str, title: &str, body_file: &str) -> Result<(), GitHubCliError> {
        self.execute(GitHubExecuteInput::new(
            cwd,
            [
                "pr",
                "create",
                "--base",
                base_branch,
                "--head",
                head_selector,
                "--title",
                title,
                "--body-file",
                body_file,
            ],
        ))
        .await
        .map(drop)
    }

    /// `getDefaultBranch`.
    pub async fn get_default_branch(&self, cwd: &str, rate_limit_host: Option<String>) -> Result<Option<String>, GitHubCliError> {
        let mut input = GitHubExecuteInput::new(cwd, ["repo", "view", "--json", "defaultBranchRef", "--jq", ".defaultBranchRef.name"]);
        input.rate_limit_host = rate_limit_host;
        let output = self.execute(input).await?;
        let trimmed = js_trim(&output.stdout);
        Ok((!trimmed.is_empty()).then(|| trimmed.to_owned()))
    }

    /// `checkoutPullRequest`.
    pub async fn checkout_pull_request(&self, cwd: &str, reference: &str, force: bool) -> Result<(), GitHubCliError> {
        let mut args = vec!["pr".to_owned(), "checkout".into(), reference.to_owned()];
        if force {
            args.push("--force".into());
        }
        self.execute(GitHubExecuteInput::new(cwd, args)).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_owned()).collect()
    }

    #[test]
    fn reads_command_hosts() {
        assert_eq!(
            command_hosts(&s(&["api", "user", "--hostname", "Other.Example.Test"])),
            vec![Some("other.example.test".into())]
        );
        assert_eq!(command_hosts(&s(&["pr", "view", "1", "-Rhost.test/o/r"])), vec![Some("host.test".into())]);
        assert_eq!(command_hosts(&s(&["repo", "view", "o/r"])), vec![None]);
        assert_eq!(
            command_hosts(&s(&["api", "https://other.example.test/user", "--hostname", "github.com"])),
            vec![Some("other.example.test".into()), Some("github.com".into())]
        );
        assert!(command_hosts(&s(&["api", "user"])).is_empty());
    }

    #[test]
    fn derives_clone_urls_from_create_output() {
        let urls = derive_repository_clone_urls_from_create_output(
            "✓ Created repository octocat/demo on github.com\nhttps://github.com/octocat/demo\n",
            "octocat/demo",
        );
        assert_eq!(urls.url, "https://github.com/octocat/demo");
        assert_eq!(urls.ssh_url, "git@github.com:octocat/demo.git");
        let fallback = derive_repository_clone_urls_from_create_output("", "octocat/demo");
        assert_eq!(fallback.url, "https://github.com/octocat/demo");
    }
}
