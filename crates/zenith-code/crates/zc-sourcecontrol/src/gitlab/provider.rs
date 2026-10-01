//! `GitLabSourceControlProvider.ts`: the GitLab provider over [`GitLabCli`], and its discovery spec.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use futures::FutureExt;
use regex::Regex;
use zc_contracts::{
    ChangeRequest, SourceControlProviderAuth, SourceControlProviderAuthStatus as Auth, SourceControlProviderInfo, SourceControlProviderKind,
    SourceControlRepositoryCloneUrls,
};

use crate::discovery::{
    combined_auth_output, first_safe_auth_line, match_first, parse_cli_host, provider_auth, AuthProbeInput, CliDiscoverySpec, RefinementInput,
};
use crate::errors::{Cause, SourceControlProviderError};
use crate::github::cli::SchemaDecodeError;
use crate::github::provider::decode_link_subject;
use crate::gitlab::auth_status::{find_authenticated_gitlab_host, parse_gitlab_auth_status_hosts};
use crate::gitlab::cli::{GitLabCli, GitLabCliError, GitLabExecuteInput};
use crate::provider::*;
use crate::records::NormalizedChangeRequest;
use crate::util::{encode_uri_component, url_host};

const KIND: SourceControlProviderKind = SourceControlProviderKind::Gitlab;

fn provider_error(operation: &str, cwd: &str, error: GitLabCliError) -> SourceControlProviderError {
    SourceControlProviderError::new(KIND, operation, cwd, error.detail())
        .with_command(error.command())
        .with_cause(Cause::new(error))
}

/// GitLab's `toChangeRequest`.
pub fn to_change_request(record: &NormalizedChangeRequest) -> ChangeRequest {
    let mut change = record.to_change_request(KIND);
    change.closed_at = Some(record.closed_at.clone().flatten());
    change.merged_at = Some(record.merged_at.clone().flatten());
    change
}

/// `parseGitLabAuth`.
pub fn parse_gitlab_auth(input: &AuthProbeInput) -> SourceControlProviderAuth {
    static PATTERNS: OnceLock<[Regex; 3]> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        [
            Regex::new(r"(?i)Logged in to .* as\s+([^\s(]+)").expect("valid regex"),
            Regex::new(r"(?i)Logged in to .* account\s+([^\s(]+)").expect("valid regex"),
            Regex::new(r"(?i)account:\s*([^\s(]+)").expect("valid regex"),
        ]
    });
    let output = combined_auth_output(input);
    let hosts = parse_gitlab_auth_status_hosts(&output);
    let authenticated = find_authenticated_gitlab_host(&hosts);
    let account = authenticated
        .and_then(|h| h.account.clone())
        .or_else(|| match_first(&output, &patterns.iter().collect::<Vec<_>>()));
    let host = authenticated.map(|h| h.host.clone()).or_else(|| parse_cli_host(&output));
    if let Some(account) = account {
        return provider_auth(Auth::Authenticated, Some(&account), host.as_deref(), None);
    }
    let line = first_safe_auth_line(&output);
    if input.exit_code != 0 {
        return provider_auth(
            Auth::Unauthenticated,
            None,
            host.as_deref(),
            Some(line.as_deref().unwrap_or("Run `glab auth login` to authenticate GitLab CLI.")),
        );
    }
    provider_auth(
        Auth::Unknown,
        None,
        host.as_deref(),
        Some(line.as_deref().unwrap_or("GitLab CLI auth status could not be parsed.")),
    )
}

/// `refineUnknownGitLabRemote`: a remote is GitLab when `glab` is signed in to its host.
pub fn refine_unknown_gitlab_remote(input: &RefinementInput) -> Option<SourceControlProviderInfo> {
    let host = input.context.provider.name.to_lowercase();
    let authenticated = parse_gitlab_auth_status_hosts(&combined_auth_output(&input.auth))
        .iter()
        .any(|entry| entry.account.is_some() && entry.host == host);
    authenticated.then(|| SourceControlProviderInfo {
        kind: KIND,
        name: "GitLab Self-Hosted".into(),
        base_url: input.context.provider.base_url.clone(),
    })
}

/// The GitLab discovery spec.
pub fn discovery() -> CliDiscoverySpec {
    CliDiscoverySpec {
        kind: KIND,
        label: "GitLab".into(),
        install_hint:
            "Install the GitLab command-line tool (`glab`) from https://gitlab.com/gitlab-org/cli or your package manager (for example `brew install glab`)."
                .into(),
        executable: "glab".into(),
        version_args: vec!["--version".into()],
        auth_args: vec!["auth".into(), "status".into()],
        remote_refinement_args: None,
        probe_timeout_ms: None,
        parse_auth: Arc::new(parse_gitlab_auth),
        refine_unknown_remote: Some(Arc::new(refine_unknown_gitlab_remote)),
    }
}

/// `GitLabSourceControlProvider`.
#[derive(Clone)]
pub struct GitLabSourceControlProvider {
    gitlab: GitLabCli,
}

impl GitLabSourceControlProvider {
    pub fn new(gitlab: GitLabCli) -> Self {
        Self { gitlab }
    }

    pub fn cli(&self) -> &GitLabCli {
        &self.gitlab
    }

    async fn read_link_subject(gitlab: GitLabCli, cwd: String, host: String, endpoint: String) -> Result<SourceControlLinkSubject, SourceControlProviderError> {
        let mut input = GitLabExecuteInput::new(cwd.clone(), ["api", "--hostname", host.as_str(), endpoint.as_str()]);
        input.timeout_ms = Some(3_000);
        input.max_output_bytes = Some(32_000);
        let output = gitlab.execute(input).await.map_err(|cause| {
            SourceControlProviderError::new(KIND, "resolveLink", &cwd, "The linked subject could not be read.").with_cause(Cause::new(cause))
        })?;
        decode_link_subject(&output.stdout, "description").map_err(|e| {
            SourceControlProviderError::new(KIND, "resolveLink.decode", &cwd, "The linked subject could not be read.")
                .with_cause(Cause::new(SchemaDecodeError(e)))
        })
    }
}

#[async_trait]
impl SourceControlProvider for GitLabSourceControlProvider {
    fn kind(&self) -> SourceControlProviderKind {
        KIND
    }

    fn resolve_link(&self, cwd: &str, url: &url::Url) -> Option<LinkLookup> {
        static PATH: OnceLock<Regex> = OnceLock::new();
        // Automatic enrichment must not send ambient CLI credentials to a host from message text.
        if url_host(url) != "gitlab.com" {
            return None;
        }
        let pattern = PATH.get_or_init(|| Regex::new(r"^/(.+)/-/(merge_requests|issues)/([1-9][0-9]*)(?:/.*)?$").expect("valid regex"));
        let captures = pattern.captures(url.path())?;
        let endpoint = format!("projects/{}/{}/{}", encode_uri_component(&captures[1]), &captures[2], &captures[3]);
        Some(Self::read_link_subject(self.gitlab.clone(), cwd.to_owned(), url_host(url), endpoint).boxed())
    }

    async fn list_change_requests(&self, input: ListChangeRequestsInput) -> Result<Vec<ChangeRequest>, SourceControlProviderError> {
        let source = source_control_ref_from_input(&input.head_selector, input.source.as_ref());
        self.gitlab
            .list_merge_requests(&input.cwd, &input.head_selector, source.as_ref(), input.state, input.limit)
            .await
            .map(|items| items.iter().map(to_change_request).collect())
            .map_err(|error| {
                provider_error("listChangeRequests", &input.cwd, error).with_reference(transport_safe_source_control_error_value(&input.head_selector))
            })
    }

    async fn get_change_request(&self, input: GetChangeRequestInput) -> Result<ChangeRequest, SourceControlProviderError> {
        self.gitlab
            .get_merge_request(&input.cwd, &input.reference)
            .await
            .map(|record| to_change_request(&record))
            .map_err(|error| provider_error("getChangeRequest", &input.cwd, error).with_reference(transport_safe_source_control_error_value(&input.reference)))
    }

    async fn create_change_request(&self, input: CreateChangeRequestInput) -> Result<(), SourceControlProviderError> {
        let source = source_control_ref_from_input(&input.head_selector, input.source.as_ref());
        self.gitlab
            .create_merge_request(
                &input.cwd,
                &input.base_ref_name,
                &input.head_selector,
                source.as_ref(),
                input.target.as_ref(),
                &input.title,
                &input.body_file,
            )
            .await
            .map_err(|error| {
                provider_error("createChangeRequest", &input.cwd, error).with_reference(transport_safe_source_control_error_value(&input.head_selector))
            })
    }

    async fn get_repository_clone_urls(&self, input: RepositoryCloneUrlsInput) -> Result<SourceControlRepositoryCloneUrls, SourceControlProviderError> {
        self.gitlab.get_repository_clone_urls(&input.cwd, &input.repository).await.map_err(|error| {
            provider_error("getRepositoryCloneUrls", &input.cwd, error).with_repository(transport_safe_source_control_error_value(&input.repository))
        })
    }

    async fn create_repository(&self, input: CreateRepositoryInput) -> Result<SourceControlRepositoryCloneUrls, SourceControlProviderError> {
        self.gitlab
            .create_repository(&input.cwd, &input.repository, input.visibility)
            .await
            .map_err(|error| {
                provider_error("createRepository", &input.cwd, error).with_repository(transport_safe_source_control_error_value(&input.repository))
            })
    }

    async fn get_default_branch(&self, input: DefaultBranchInput) -> Result<Option<String>, SourceControlProviderError> {
        self.gitlab
            .get_default_branch(&input.cwd)
            .await
            .map_err(|error| provider_error("getDefaultBranch", &input.cwd, error))
    }

    async fn checkout_change_request(&self, input: CheckoutChangeRequestInput) -> Result<(), SourceControlProviderError> {
        self.gitlab.checkout_merge_request(&input.cwd, &input.reference).await.map_err(|error| {
            provider_error("checkoutChangeRequest", &input.cwd, error).with_reference(transport_safe_source_control_error_value(&input.reference))
        })
    }
}
