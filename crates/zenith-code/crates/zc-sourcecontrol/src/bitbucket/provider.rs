//! `BitbucketSourceControlProvider.ts`: the Bitbucket provider over [`BitbucketApi`], and its API
//! discovery spec.

use std::sync::Arc;

use async_trait::async_trait;
use futures::FutureExt;
use zc_contracts::{ChangeRequest, SourceControlProviderKind, SourceControlRepositoryCloneUrls};

use crate::bitbucket::api::{BitbucketApi, BitbucketApiError};
use crate::discovery::ApiDiscoverySpec;
use crate::errors::{Cause, SourceControlProviderError};
use crate::provider::*;
use crate::records::NormalizedChangeRequest;

const KIND: SourceControlProviderKind = SourceControlProviderKind::Bitbucket;

fn provider_error(operation: &str, cwd: &str, detail: &str, error: BitbucketApiError) -> SourceControlProviderError {
    SourceControlProviderError::new(KIND, operation, cwd, detail).with_cause(Cause::new(error))
}

/// Bitbucket's `toChangeRequest` (no `closedAt`/`mergedAt`).
pub fn to_change_request(record: &NormalizedChangeRequest) -> ChangeRequest {
    record.to_change_request(KIND)
}

/// `makeDiscovery`: the API probe.
pub fn discovery(api: BitbucketApi) -> ApiDiscoverySpec {
    ApiDiscoverySpec {
        kind: KIND,
        label: "Bitbucket".into(),
        install_hint: "Add a Bitbucket token in Settings → Source Control.".into(),
        probe_auth: Arc::new(move || {
            let api = api.clone();
            async move { api.probe_auth().await }.boxed()
        }),
    }
}

/// `BitbucketSourceControlProvider`.
#[derive(Clone)]
pub struct BitbucketSourceControlProvider {
    api: BitbucketApi,
}

impl BitbucketSourceControlProvider {
    pub fn new(api: BitbucketApi) -> Self {
        Self { api }
    }

    pub fn api(&self) -> &BitbucketApi {
        &self.api
    }
}

#[async_trait]
impl SourceControlProvider for BitbucketSourceControlProvider {
    fn kind(&self) -> SourceControlProviderKind {
        KIND
    }

    async fn list_change_requests(&self, input: ListChangeRequestsInput) -> Result<Vec<ChangeRequest>, SourceControlProviderError> {
        let source = source_control_ref_from_input(&input.head_selector, input.source.as_ref());
        self.api
            .list_pull_requests(
                &input.cwd,
                input.context.as_ref(),
                &input.head_selector,
                source.as_ref(),
                input.state,
                input.limit,
            )
            .await
            .map(|items| items.iter().map(to_change_request).collect())
            .map_err(|error| {
                provider_error("listChangeRequests", &input.cwd, "Failed to list change requests.", error)
                    .with_reference(transport_safe_source_control_error_value(&input.head_selector))
            })
    }

    async fn get_change_request(&self, input: GetChangeRequestInput) -> Result<ChangeRequest, SourceControlProviderError> {
        self.api
            .get_pull_request(&input.cwd, input.context.as_ref(), &input.reference)
            .await
            .map(|record| to_change_request(&record))
            .map_err(|error| {
                provider_error("getChangeRequest", &input.cwd, "Failed to get change request.", error)
                    .with_reference(transport_safe_source_control_error_value(&input.reference))
            })
    }

    async fn create_change_request(&self, input: CreateChangeRequestInput) -> Result<(), SourceControlProviderError> {
        let source = source_control_ref_from_input(&input.head_selector, input.source.as_ref());
        self.api
            .create_pull_request(
                &input.cwd,
                input.context.as_ref(),
                &input.base_ref_name,
                &input.head_selector,
                source.as_ref(),
                input.target.as_ref(),
                &input.title,
                &input.body_file,
            )
            .await
            .map_err(|error| {
                provider_error("createChangeRequest", &input.cwd, "Failed to create change request.", error)
                    .with_reference(transport_safe_source_control_error_value(&input.head_selector))
            })
    }

    async fn get_repository_clone_urls(&self, input: RepositoryCloneUrlsInput) -> Result<SourceControlRepositoryCloneUrls, SourceControlProviderError> {
        self.api
            .get_repository_clone_urls(&input.cwd, input.context.as_ref(), &input.repository)
            .await
            .map_err(|error| {
                provider_error("getRepositoryCloneUrls", &input.cwd, "Failed to get repository clone URLs.", error)
                    .with_repository(transport_safe_source_control_error_value(&input.repository))
            })
    }

    async fn create_repository(&self, input: CreateRepositoryInput) -> Result<SourceControlRepositoryCloneUrls, SourceControlProviderError> {
        self.api.create_repository(&input.repository, input.visibility).await.map_err(|error| {
            provider_error("createRepository", &input.cwd, "Failed to create repository.", error)
                .with_repository(transport_safe_source_control_error_value(&input.repository))
        })
    }

    async fn get_default_branch(&self, input: DefaultBranchInput) -> Result<Option<String>, SourceControlProviderError> {
        self.api
            .get_default_branch(&input.cwd, input.context.as_ref())
            .await
            .map_err(|error| provider_error("getDefaultBranch", &input.cwd, "Failed to get default branch.", error))
    }

    async fn checkout_change_request(&self, input: CheckoutChangeRequestInput) -> Result<(), SourceControlProviderError> {
        self.api
            .checkout_pull_request(&input.cwd, input.context.as_ref(), &input.reference, input.force)
            .await
            .map_err(|error| {
                provider_error("checkoutChangeRequest", &input.cwd, "Failed to check out change request.", error)
                    .with_reference(transport_safe_source_control_error_value(&input.reference))
            })
    }
}
