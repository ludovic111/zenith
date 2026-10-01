//! `GitHubSourceControlProvider.ts`: the GitHub provider over [`GitHubCli`], and its discovery spec.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use futures::FutureExt;
use regex::Regex;
use serde_json::Value;
use zc_contracts::{
    ChangeRequest, SourceControlProviderAuth, SourceControlProviderAuthStatus as Auth, SourceControlProviderKind, SourceControlRepositoryCloneUrls,
};

use crate::discovery::{combined_auth_output, first_safe_auth_line, provider_auth, AuthProbeInput, CliDiscoverySpec};
use crate::errors::{Cause, SourceControlProviderError};
use crate::github::auth_status::{find_authenticated_github_account, parse_github_auth_status};
use crate::github::cli::{GitHubCli, GitHubCliError, GitHubCliErrorKind, GitHubExecuteInput, SchemaDecodeError, PR_VIEW_FIELDS};
use crate::github::pull_requests::decode_github_pull_request_list_json;
use crate::provider::*;
use crate::util::{js_trim, url_host};

const KIND: SourceControlProviderKind = SourceControlProviderKind::Github;

fn provider_error(operation: &str, cwd: &str, error: GitHubCliError) -> SourceControlProviderError {
    SourceControlProviderError::new(KIND, operation, cwd, error.detail())
        .with_command(error.command())
        .with_cause(Cause::new(error))
}

/// `parseGitHubAuth`.
pub fn parse_github_auth(input: &AuthProbeInput) -> SourceControlProviderAuth {
    let output = combined_auth_output(input);
    let status = parse_github_auth_status(&input.stdout);
    if let Some(account) = find_authenticated_github_account(&status.accounts) {
        return provider_auth(Auth::Authenticated, Some(&account.account), Some(&account.host), None);
    }
    let failed = status.accounts.iter().find(|a| a.active).or_else(|| status.accounts.first());
    if status.parsed {
        return provider_auth(
            Auth::Unauthenticated,
            None,
            failed.map(|a| a.host.as_str()),
            Some(
                failed
                    .and_then(|a| a.error.as_deref())
                    .unwrap_or("Run `gh auth login` to authenticate GitHub CLI with an active account."),
            ),
        );
    }
    // gh gained `auth status --json` in 2.81.0. Older versions reject the flag and exit non-zero,
    // which reads exactly like a signed-out CLI. Name the real problem instead.
    if input.exit_code != 0 && output.contains("unknown flag: --json") {
        return provider_auth(
            Auth::Unknown,
            None,
            None,
            Some("GitHub CLI is too old to report sign-in status. Update `gh` to 2.81.0 or newer (for example `brew upgrade gh`) and rescan."),
        );
    }
    let line = first_safe_auth_line(&output);
    if input.exit_code != 0 {
        return provider_auth(
            Auth::Unauthenticated,
            None,
            None,
            Some(line.as_deref().unwrap_or("Run `gh auth login` to authenticate GitHub CLI.")),
        );
    }
    provider_auth(
        Auth::Unknown,
        None,
        None,
        Some(line.as_deref().unwrap_or("GitHub CLI auth status could not be parsed.")),
    )
}

/// The GitHub discovery spec.
pub fn discovery() -> CliDiscoverySpec {
    CliDiscoverySpec {
        kind: KIND,
        label: "GitHub".into(),
        install_hint: "Install the GitHub command-line tool (`gh`) via https://cli.github.com/ or your package manager (for example `brew install gh`).".into(),
        executable: "gh".into(),
        version_args: vec!["--version".into()],
        auth_args: ["auth", "status", "--json", "hosts"].map(String::from).to_vec(),
        remote_refinement_args: None,
        probe_timeout_ms: None,
        parse_auth: Arc::new(parse_github_auth),
        refine_unknown_remote: None,
    }
}

/// `GitHubSourceControlProvider`.
#[derive(Clone)]
pub struct GitHubSourceControlProvider {
    github: GitHubCli,
}

impl GitHubSourceControlProvider {
    pub fn new(github: GitHubCli) -> Self {
        Self { github }
    }

    pub fn cli(&self) -> &GitHubCli {
        &self.github
    }

    async fn read_link_subject(github: GitHubCli, cwd: String, host: String, endpoint: String) -> Result<SourceControlLinkSubject, SourceControlProviderError> {
        let mut input = GitHubExecuteInput::new(cwd.clone(), ["api", "--hostname", host.as_str(), endpoint.as_str(), "--jq", "{title, body}"]);
        input.env = Some(BTreeMap::from([("GH_PROMPT_DISABLED".to_owned(), "1".to_owned())]));
        input.timeout_ms = Some(3_000);
        input.max_output_bytes = Some(32_000);
        let output = github.execute(input).await.map_err(|cause| {
            SourceControlProviderError::new(KIND, "resolveLink", &cwd, "The linked subject could not be read.").with_cause(Cause::new(cause))
        })?;
        decode_link_subject(&output.stdout, "body").map_err(|e| {
            SourceControlProviderError::new(KIND, "resolveLink.decode", &cwd, "The linked subject could not be read.")
                .with_cause(Cause::new(SchemaDecodeError(e)))
        })
    }
}

/// `Schema.fromJsonString(Struct({title: String, <body>: NullOr(String)}))`.
pub(crate) fn decode_link_subject(raw: &str, body_key: &str) -> Result<SourceControlLinkSubject, String> {
    let value: Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    let title = value.get("title").and_then(Value::as_str).ok_or("Missing title")?.to_owned();
    let body = match value.get(body_key) {
        Some(Value::Null) => None,
        Some(Value::String(body)) => Some(body.clone()),
        _ => return Err(format!("Missing {body_key}")),
    };
    Ok(SourceControlLinkSubject { title, body })
}

#[async_trait]
impl SourceControlProvider for GitHubSourceControlProvider {
    fn kind(&self) -> SourceControlProviderKind {
        KIND
    }

    fn resolve_link(&self, cwd: &str, url: &url::Url) -> Option<LinkLookup> {
        static PATH: OnceLock<Regex> = OnceLock::new();
        // Automatic enrichment must not send ambient CLI credentials to a host from message text.
        if url_host(url) != "github.com" {
            return None;
        }
        let pattern = PATH.get_or_init(|| Regex::new(r"^/([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+)/(?:pull|issues)/([1-9][0-9]*)(?:/.*)?$").expect("valid regex"));
        let captures = pattern.captures(url.path())?;
        let endpoint = format!("repos/{}/{}/issues/{}", &captures[1], &captures[2], &captures[3]);
        Some(Self::read_link_subject(self.github.clone(), cwd.to_owned(), url_host(url), endpoint).boxed())
    }

    async fn list_change_requests(&self, input: ListChangeRequestsInput) -> Result<Vec<ChangeRequest>, SourceControlProviderError> {
        let host = context_host(input.context.as_ref());
        let reference = transport_safe_source_control_error_value(&input.head_selector);
        let fail = |error: GitHubCliError| provider_error("listChangeRequests", &input.cwd, error).with_reference(reference.clone());
        if input.state == ChangeRequestStateFilter::Open {
            let items = self
                .github
                .list_open_pull_requests(&input.cwd, &input.head_selector, input.limit, host)
                .await
                .map_err(fail)?;
            return Ok(items.iter().map(to_change_request).collect());
        }
        let mut execute = GitHubExecuteInput::new(
            input.cwd.clone(),
            [
                "pr".to_owned(),
                "list".into(),
                "--head".into(),
                input.head_selector.clone(),
                "--state".into(),
                input.state.as_str().into(),
                "--limit".into(),
                input.limit.unwrap_or(20).to_string(),
                "--json".into(),
                PR_VIEW_FIELDS.into(),
            ],
        );
        execute.rate_limit_host = host;
        let output = self.github.execute(execute).await.map_err(fail)?;
        let raw = js_trim(&output.stdout);
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        let items = decode_github_pull_request_list_json(raw).map_err(|e| {
            fail(GitHubCliError::new(
                GitHubCliErrorKind::ChangeRequestListDecode,
                &input.cwd,
                Cause::new(SchemaDecodeError(e)),
            ))
        })?;
        Ok(items.iter().map(to_change_request).collect())
    }

    async fn get_change_request(&self, input: GetChangeRequestInput) -> Result<ChangeRequest, SourceControlProviderError> {
        let host = context_host(input.context.as_ref());
        self.github
            .get_pull_request(&input.cwd, &input.reference, host)
            .await
            .map(|record| to_change_request(&record))
            .map_err(|error| provider_error("getChangeRequest", &input.cwd, error).with_reference(transport_safe_source_control_error_value(&input.reference)))
    }

    async fn create_change_request(&self, input: CreateChangeRequestInput) -> Result<(), SourceControlProviderError> {
        self.github
            .create_pull_request(&input.cwd, &input.base_ref_name, &input.head_selector, &input.title, &input.body_file)
            .await
            .map_err(|error| {
                provider_error("createChangeRequest", &input.cwd, error).with_reference(transport_safe_source_control_error_value(&input.head_selector))
            })
    }

    async fn get_repository_clone_urls(&self, input: RepositoryCloneUrlsInput) -> Result<SourceControlRepositoryCloneUrls, SourceControlProviderError> {
        self.github.get_repository_clone_urls(&input.cwd, &input.repository).await.map_err(|error| {
            provider_error("getRepositoryCloneUrls", &input.cwd, error).with_repository(transport_safe_source_control_error_value(&input.repository))
        })
    }

    async fn create_repository(&self, input: CreateRepositoryInput) -> Result<SourceControlRepositoryCloneUrls, SourceControlProviderError> {
        self.github
            .create_repository(&input.cwd, &input.repository, input.visibility)
            .await
            .map_err(|error| {
                provider_error("createRepository", &input.cwd, error).with_repository(transport_safe_source_control_error_value(&input.repository))
            })
    }

    async fn get_default_branch(&self, input: DefaultBranchInput) -> Result<Option<String>, SourceControlProviderError> {
        let host = context_host(input.context.as_ref());
        self.github
            .get_default_branch(&input.cwd, host)
            .await
            .map_err(|error| provider_error("getDefaultBranch", &input.cwd, error))
    }

    async fn checkout_change_request(&self, input: CheckoutChangeRequestInput) -> Result<(), SourceControlProviderError> {
        self.github
            .checkout_pull_request(&input.cwd, &input.reference, input.force)
            .await
            .map_err(|error| {
                provider_error("checkoutChangeRequest", &input.cwd, error).with_reference(transport_safe_source_control_error_value(&input.reference))
            })
    }
}

/// GitHub's `toChangeRequest`: `state` defaults to open; `closedAt`/`mergedAt` are always present.
pub fn to_change_request(record: &crate::records::NormalizedChangeRequest) -> ChangeRequest {
    let mut change = record.to_change_request(KIND);
    change.closed_at = Some(record.closed_at.clone().flatten());
    change.merged_at = Some(record.merged_at.clone().flatten());
    change
}
