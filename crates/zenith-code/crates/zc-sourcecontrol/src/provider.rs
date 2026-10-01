//! `SourceControlProvider.ts`: the provider-neutral API every forge implements, its inputs, and
//! the helpers for owner-qualified head selectors and transport-safe error values.

use std::sync::OnceLock;

use async_trait::async_trait;
use futures::future::BoxFuture;
use regex::Regex;
use zc_contracts::{ChangeRequest, SourceControlProviderInfo, SourceControlProviderKind, SourceControlRepositoryCloneUrls, SourceControlRepositoryVisibility};

use crate::errors::SourceControlProviderError;
use crate::util::{js_slice_units, js_trim, parse_url, url_href};

/// `SourceControlLinkSubject`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceControlLinkSubject {
    pub title: String,
    pub body: Option<String>,
}

/// A pending link lookup (`resolveLink` returns `undefined` synchronously for URLs it does not
/// handle, and an effect otherwise).
pub type LinkLookup = BoxFuture<'static, Result<SourceControlLinkSubject, SourceControlProviderError>>;

/// `SourceControlProviderContext`.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceControlProviderContext {
    pub provider: SourceControlProviderInfo,
    pub remote_name: String,
    pub remote_url: String,
    /// An explicit web authority can disambiguate Forgejo logins sharing an SSH alias.
    pub requested_host: Option<String>,
}

/// `SourceControlRefSelector`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SourceControlRefSelector {
    pub ref_name: String,
    pub owner: Option<String>,
    pub repository: Option<String>,
}

/// `ChangeRequestState | "all"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeRequestStateFilter {
    Open,
    Closed,
    Merged,
    All,
}

impl ChangeRequestStateFilter {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Merged => "merged",
            Self::All => "all",
        }
    }
}

/// `listChangeRequests` input.
#[derive(Debug, Clone)]
pub struct ListChangeRequestsInput {
    pub cwd: String,
    pub context: Option<SourceControlProviderContext>,
    pub source: Option<SourceControlRefSelector>,
    pub head_selector: String,
    pub state: ChangeRequestStateFilter,
    pub limit: Option<u32>,
}

/// `getChangeRequest` input.
#[derive(Debug, Clone)]
pub struct GetChangeRequestInput {
    pub cwd: String,
    pub context: Option<SourceControlProviderContext>,
    pub reference: String,
}

/// `createChangeRequest` input.
#[derive(Debug, Clone)]
pub struct CreateChangeRequestInput {
    pub cwd: String,
    pub context: Option<SourceControlProviderContext>,
    pub source: Option<SourceControlRefSelector>,
    pub target: Option<SourceControlRefSelector>,
    pub base_ref_name: String,
    pub head_selector: String,
    pub title: String,
    pub body_file: String,
}

/// `getRepositoryCloneUrls` input.
#[derive(Debug, Clone)]
pub struct RepositoryCloneUrlsInput {
    pub cwd: String,
    pub context: Option<SourceControlProviderContext>,
    pub repository: String,
}

/// `createRepository` input.
#[derive(Debug, Clone)]
pub struct CreateRepositoryInput {
    pub cwd: String,
    pub repository: String,
    pub visibility: SourceControlRepositoryVisibility,
}

/// `getDefaultBranch` input.
#[derive(Debug, Clone)]
pub struct DefaultBranchInput {
    pub cwd: String,
    pub context: Option<SourceControlProviderContext>,
}

/// `checkoutChangeRequest` input.
#[derive(Debug, Clone)]
pub struct CheckoutChangeRequestInput {
    pub cwd: String,
    pub context: Option<SourceControlProviderContext>,
    pub reference: String,
    pub force: bool,
}

/// `SourceControlProvider`.
#[async_trait]
pub trait SourceControlProvider: Send + Sync {
    fn kind(&self) -> SourceControlProviderKind;

    /// `resolveLink`: `None` (synchronously) for URLs this provider does not read.
    fn resolve_link(&self, _cwd: &str, _url: &url::Url) -> Option<LinkLookup> {
        None
    }

    async fn list_change_requests(&self, input: ListChangeRequestsInput) -> Result<Vec<ChangeRequest>, SourceControlProviderError>;
    async fn get_change_request(&self, input: GetChangeRequestInput) -> Result<ChangeRequest, SourceControlProviderError>;
    async fn create_change_request(&self, input: CreateChangeRequestInput) -> Result<(), SourceControlProviderError>;
    async fn get_repository_clone_urls(&self, input: RepositoryCloneUrlsInput) -> Result<SourceControlRepositoryCloneUrls, SourceControlProviderError>;
    async fn create_repository(&self, input: CreateRepositoryInput) -> Result<SourceControlRepositoryCloneUrls, SourceControlProviderError>;
    async fn get_default_branch(&self, input: DefaultBranchInput) -> Result<Option<String>, SourceControlProviderError>;
    async fn checkout_change_request(&self, input: CheckoutChangeRequestInput) -> Result<(), SourceControlProviderError>;
}

const MAX_ERROR_TRANSPORT_VALUE_LENGTH: usize = 256;

/// `transportSafeSourceControlErrorValue`: strips URL secrets and bounds diagnostic values sent
/// over transport.
pub fn transport_safe_source_control_error_value(value: &str) -> String {
    static WHITESPACE: OnceLock<Regex> = OnceLock::new();
    let printable: String = value.chars().map(|c| if (c as u32) < 32 || c as u32 == 127 { ' ' } else { c }).collect();
    let normalized = WHITESPACE
        .get_or_init(|| Regex::new(r"\s+").expect("valid regex"))
        .replace_all(js_trim(&printable), " ")
        .into_owned();
    let safe = match parse_url(&normalized) {
        Some(mut url) => {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            url_href(&url)
        }
        None => normalized,
    };
    js_slice_units(&safe, MAX_ERROR_TRANSPORT_VALUE_LENGTH).to_owned()
}

/// `parseSourceControlOwnerRef`: `owner:branch`.
pub fn parse_source_control_owner_ref(head_selector: &str) -> Option<SourceControlRefSelector> {
    static OWNER_REF: OnceLock<Regex> = OnceLock::new();
    let pattern = OWNER_REF.get_or_init(|| Regex::new(r"^([^:/\s]+):(.+)$").expect("valid regex"));
    let captures = pattern.captures(js_trim(head_selector))?;
    let owner = js_trim(captures.get(1)?.as_str());
    let ref_name = js_trim(captures.get(2)?.as_str());
    (!owner.is_empty() && !ref_name.is_empty()).then(|| SourceControlRefSelector {
        ref_name: ref_name.to_owned(),
        owner: Some(owner.to_owned()),
        repository: None,
    })
}

/// `sourceBranch`.
pub fn source_branch(head_selector: &str, source: Option<&SourceControlRefSelector>) -> String {
    match source {
        Some(source) => source.ref_name.clone(),
        None => parse_source_control_owner_ref(head_selector)
            .map(|selector| selector.ref_name)
            .unwrap_or_else(|| js_trim(head_selector).to_owned()),
    }
}

/// `sourceControlRefFromInput`.
pub fn source_control_ref_from_input(head_selector: &str, source: Option<&SourceControlRefSelector>) -> Option<SourceControlRefSelector> {
    source.cloned().or_else(|| parse_source_control_owner_ref(head_selector))
}

/// `new URL(baseUrl).host` of a context's provider, used as the GitHub rate-limit host.
pub(crate) fn context_host(context: Option<&SourceControlProviderContext>) -> Option<String> {
    let context = context?;
    parse_url(&context.provider.base_url).map(|url| crate::util::url_host(&url))
}

#[cfg(test)]
mod tests {
    use super::*;

    // SourceControlProvider.test.ts
    #[test]
    fn removes_url_credentials_query_parameters_and_fragments() {
        assert_eq!(
            transport_safe_source_control_error_value("https://user:secret@example.test/org/repo/pull/42?token=secret#discussion"),
            "https://example.test/org/repo/pull/42"
        );
    }

    #[test]
    fn normalizes_control_characters_and_bounds_values() {
        let input = format!("  owner/repo\n\t{}  ", "x".repeat(300));
        assert_eq!(transport_safe_source_control_error_value(&input), format!("owner/repo {}", "x".repeat(245)));
    }

    #[test]
    fn parses_owner_refs() {
        let selector = parse_source_control_owner_ref(" fork:feature/x ").unwrap();
        assert_eq!(selector.owner.as_deref(), Some("fork"));
        assert_eq!(selector.ref_name, "feature/x");
        assert!(parse_source_control_owner_ref("feature/x").is_none());
        assert_eq!(source_branch("fork:feature/x", None), "feature/x");
        assert_eq!(source_branch(" main ", None), "main");
    }
}
