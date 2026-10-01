//! `AzureDevOpsSourceControlProvider.ts`.

use std::sync::Arc;

use async_trait::async_trait;
use zc_contracts::{
    ChangeRequest, SourceControlProviderAuth, SourceControlProviderAuthStatus as Auth, SourceControlProviderKind, SourceControlRepositoryCloneUrls,
};

use crate::azure::cli::{AzureDevOpsCli, AzureDevOpsCliError};
use crate::discovery::{combined_auth_output, first_safe_auth_line, provider_auth, AuthProbeInput, CliDiscoverySpec};
use crate::errors::{Cause, SourceControlProviderError};
use crate::provider::*;
use crate::records::NormalizedChangeRequest;
use crate::util::{js_trim, split_lines};

const KIND: SourceControlProviderKind = SourceControlProviderKind::AzureDevops;

fn provider_error(operation: &str, cwd: &str, error: AzureDevOpsCliError) -> SourceControlProviderError {
    SourceControlProviderError::new(KIND, operation, cwd, error.detail())
        .with_command(error.command())
        .with_cause(Cause::new(error))
}

/// `parseAzureAuth`: `az account show --query user.name -o tsv`.
pub fn parse_azure_auth(input: &AuthProbeInput) -> SourceControlProviderAuth {
    let account = split_lines(js_trim(&input.stdout)).next().map(js_trim).unwrap_or_default().to_owned();
    if input.exit_code != 0 {
        let line = first_safe_auth_line(&combined_auth_output(input));
        return provider_auth(
            Auth::Unauthenticated,
            None,
            None,
            Some(line.as_deref().unwrap_or("Run `az login` to authenticate Azure CLI.")),
        );
    }
    if !account.is_empty() {
        return provider_auth(Auth::Authenticated, Some(&account), Some("dev.azure.com"), None);
    }
    provider_auth(
        Auth::Unknown,
        None,
        Some("dev.azure.com"),
        Some("Azure CLI account status could not be parsed."),
    )
}

/// The Azure DevOps discovery spec. `az` boots a Python interpreter on every call, so its probes
/// get 20 s instead of 5.
pub fn discovery() -> CliDiscoverySpec {
    CliDiscoverySpec {
        kind: KIND,
        label: "Azure DevOps".into(),
        install_hint: "Install the Azure command-line tools (`az`), then enable Azure DevOps support with `az extension add --name azure-devops`.".into(),
        executable: "az".into(),
        version_args: vec!["--version".into()],
        auth_args: ["account", "show", "--query", "user.name", "-o", "tsv"].map(String::from).to_vec(),
        remote_refinement_args: None,
        probe_timeout_ms: Some(20_000),
        parse_auth: Arc::new(parse_azure_auth),
        refine_unknown_remote: None,
    }
}

/// Azure's `toChangeRequest`: never cross-repository.
pub fn to_change_request(record: &NormalizedChangeRequest) -> ChangeRequest {
    let mut change = record.to_change_request(KIND);
    change.closed_at = Some(record.closed_at.clone().flatten());
    change.merged_at = Some(record.merged_at.clone().flatten());
    change.is_cross_repository = Some(false);
    change.head_repository_name_with_owner = None;
    change.head_repository_owner_login = None;
    change
}

/// `AzureDevOpsSourceControlProvider`.
#[derive(Clone)]
pub struct AzureDevOpsSourceControlProvider {
    azure: AzureDevOpsCli,
}

impl AzureDevOpsSourceControlProvider {
    pub fn new(azure: AzureDevOpsCli) -> Self {
        Self { azure }
    }

    pub fn cli(&self) -> &AzureDevOpsCli {
        &self.azure
    }
}

#[async_trait]
impl SourceControlProvider for AzureDevOpsSourceControlProvider {
    fn kind(&self) -> SourceControlProviderKind {
        KIND
    }

    async fn list_change_requests(&self, input: ListChangeRequestsInput) -> Result<Vec<ChangeRequest>, SourceControlProviderError> {
        let source = source_control_ref_from_input(&input.head_selector, input.source.as_ref());
        self.azure
            .list_pull_requests(&input.cwd, &input.head_selector, source.as_ref(), input.state, input.limit)
            .await
            .map(|items| items.iter().map(to_change_request).collect())
            .map_err(|error| {
                provider_error("listChangeRequests", &input.cwd, error).with_reference(transport_safe_source_control_error_value(&input.head_selector))
            })
    }

    async fn get_change_request(&self, input: GetChangeRequestInput) -> Result<ChangeRequest, SourceControlProviderError> {
        self.azure
            .get_pull_request(&input.cwd, &input.reference)
            .await
            .map(|record| to_change_request(&record))
            .map_err(|error| provider_error("getChangeRequest", &input.cwd, error).with_reference(transport_safe_source_control_error_value(&input.reference)))
    }

    async fn create_change_request(&self, input: CreateChangeRequestInput) -> Result<(), SourceControlProviderError> {
        let source = source_control_ref_from_input(&input.head_selector, input.source.as_ref());
        self.azure
            .create_pull_request(
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
        self.azure.get_repository_clone_urls(&input.cwd, &input.repository).await.map_err(|error| {
            provider_error("getRepositoryCloneUrls", &input.cwd, error).with_repository(transport_safe_source_control_error_value(&input.repository))
        })
    }

    async fn create_repository(&self, input: CreateRepositoryInput) -> Result<SourceControlRepositoryCloneUrls, SourceControlProviderError> {
        self.azure
            .create_repository(&input.cwd, &input.repository, input.visibility)
            .await
            .map_err(|error| {
                provider_error("createRepository", &input.cwd, error).with_repository(transport_safe_source_control_error_value(&input.repository))
            })
    }

    async fn get_default_branch(&self, input: DefaultBranchInput) -> Result<Option<String>, SourceControlProviderError> {
        self.azure
            .get_default_branch(&input.cwd)
            .await
            .map_err(|error| provider_error("getDefaultBranch", &input.cwd, error))
    }

    async fn checkout_change_request(&self, input: CheckoutChangeRequestInput) -> Result<(), SourceControlProviderError> {
        let remote_name = input.context.as_ref().map(|context| context.remote_name.as_str());
        self.azure
            .checkout_pull_request(&input.cwd, &input.reference, remote_name)
            .await
            .map_err(|error| {
                provider_error("checkoutChangeRequest", &input.cwd, error).with_reference(transport_safe_source_control_error_value(&input.reference))
            })
    }
}
