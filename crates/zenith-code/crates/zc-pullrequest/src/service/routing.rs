//! Routing: who the signed-in account on a host is (`routing`, `routingIdentity`), and running an
//! operation under a verified, pinned credential (`withRoutingCredential`).
//!
//! The Effect `routingCredential` reference is the task-local [`ROUTING_CREDENTIAL`]. Work the TS
//! forks with the caller's context (stale-while-revalidate refreshes, shared cache lookups) keeps
//! it through [`within`] / [`inherit`].

use std::future::Future;
use std::sync::Arc;

use futures::future::{BoxFuture, FutureExt};
use zc_contracts::{
    LitGithub, PullRequestRef, PullRequestRoutingIdentityInput, PullRequestRoutingIdentityResult, PullRequestRoutingResult, PullRequestUnavailableReason,
    SourceControlProviderKind,
};
use zc_sourcecontrol::util::js_trim;

use super::projects::ProjectFilter;
use super::refs::CredRef;
use super::PullRequestService;
use crate::error::PullRequestError;
use crate::provider::CredentialScope;
use crate::util::lower;

/// The verified credential a routed operation runs under.
#[derive(Clone)]
pub struct RoutingCredentialInfo {
    /// What the credential's cached reads are filed under.
    pub credential_fingerprint: String,
    /// Who the credential signs in as.
    pub viewer: String,
    /// The scope that pins host calls to the credential.
    pub(crate) scope: Arc<dyn CredentialScope>,
}

impl std::fmt::Debug for RoutingCredentialInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoutingCredentialInfo").field("viewer", &self.viewer).finish_non_exhaustive()
    }
}

tokio::task_local! {
    /// `routingCredential`: the verified credential of the operation being run, if any.
    static ROUTING_CREDENTIAL: RoutingCredentialInfo;
}

/// The routing credential in scope.
pub(crate) fn current_routing_credential() -> Option<RoutingCredentialInfo> {
    ROUTING_CREDENTIAL.try_with(Clone::clone).ok()
}

/// Runs `future` under `credential` (and the credential's own pinning scope), like a fiber given
/// the context it was forked from.
pub(crate) async fn within<F>(credential: Option<RoutingCredentialInfo>, future: F) -> F::Output
where
    F: Future + Send,
    F::Output: Send,
{
    match credential {
        None => future.await,
        Some(credential) => {
            let scope = credential.scope.clone();
            let mut slot = None;
            {
                let slot = &mut slot;
                let scoped: BoxFuture<'_, ()> = ROUTING_CREDENTIAL
                    .scope(credential, async move {
                        *slot = Some(future.await);
                    })
                    .boxed();
                scope.run(scoped).await;
            }
            slot.expect("the credential scope ran the operation")
        }
    }
}

/// `future`, boxed with the caller's routing credential captured now (for work polled elsewhere:
/// a shared cache lookup or a forked refresh).
pub(crate) fn inherit<F>(future: F) -> BoxFuture<'static, F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send,
{
    let credential = current_routing_credential();
    within(credential, future).boxed()
}

/// Forks `future` as its own task, with the caller's routing credential (`runFork` with
/// `Effect.provideContext(caller)`).
pub(crate) fn spawn_inheriting<F>(future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(inherit(future));
}

fn rejected() -> PullRequestError {
    PullRequestError::operation("routeIdentity", "The GitHub account could not be verified before starting the operation.")
}

impl PullRequestService {
    /// `credentialCached`'s reference: canonical, filed under the routing credential in scope.
    pub(crate) async fn credential_ref(&self, input: &PullRequestRef) -> Result<CredRef, PullRequestError> {
        let reference = self.canonical_ref(input).await?;
        Ok(CredRef {
            reference,
            credential: current_routing_credential().map(|credential| credential.credential_fingerprint),
        })
    }

    pub(crate) async fn routing_identity_impl(&self, input: &PullRequestRoutingIdentityInput) -> Result<PullRequestRoutingIdentityResult, PullRequestError> {
        let host = lower(&input.host);
        let projects = self
            .list_workspace_projects(ProjectFilter {
                host: Some(host.clone()),
                ..ProjectFilter::default()
            })
            .await?;
        let project = projects
            .supported
            .iter()
            .find(|candidate| candidate.api.kind() == SourceControlProviderKind::Github);
        let api = self.inner.registry.get(SourceControlProviderKind::Github);
        let (Some(project), Some(api)) = (project, api.filter(|api| api.optional_methods().get_routing_identity)) else {
            return Err(PullRequestError::unavailable(PullRequestUnavailableReason::ProviderUnsupported));
        };
        let identity = api
            .get_routing_identity(&project.project.workspace_root, &host)
            .await
            .map_err(|error| PullRequestError::from_provider("routeIdentity", error))?;
        Ok(PullRequestRoutingIdentityResult {
            account_id: identity.account_id,
            host,
            provider: LitGithub,
            viewer: identity.viewer,
        })
    }

    pub(crate) async fn with_routing_credential_impl<T: Send, F: Future<Output = Result<T, PullRequestError>> + Send>(
        &self,
        input: &PullRequestRef,
        operation: F,
    ) -> Result<T, PullRequestError> {
        let Some(expected) = input.expected_account_id.clone() else {
            return operation.await;
        };
        let project = self.require_project(input).await.map_err(|_| rejected())?;
        let api = if project.api.kind() == SourceControlProviderKind::Github {
            self.inner.registry.get(SourceControlProviderKind::Github)
        } else {
            None
        };
        let api = api.filter(|api| api.optional_methods().with_verified_credential);
        let same_host = input.host.as_deref().map(lower).as_deref() == Some(lower(&project.host).as_str());
        let Some(api) = api.filter(|_| same_host) else {
            return Err(rejected());
        };
        let credential = api
            .verified_credential(&project.project.workspace_root, &project.host)
            .await
            .map_err(|_| rejected())?;
        if credential.identity.account_id != expected {
            return Err(rejected());
        }
        let info = RoutingCredentialInfo {
            credential_fingerprint: credential.identity.credential_fingerprint.clone(),
            viewer: credential.identity.viewer.clone(),
            scope: credential.scope.clone(),
        };
        within(Some(info), operation).await
    }

    pub(crate) async fn routing_impl(&self, input: &PullRequestRef) -> Result<PullRequestRoutingResult, PullRequestError> {
        let project = self.require_project(input).await?;
        let api = if project.api.kind() == SourceControlProviderKind::Github {
            self.inner.registry.get(SourceControlProviderKind::Github)
        } else {
            None
        };
        let Some(api) = api.filter(|api| api.optional_methods().get_routing_identity) else {
            return Err(PullRequestError::unavailable(PullRequestUnavailableReason::ProviderUnsupported));
        };
        let identity = api
            .get_routing_identity(&project.project.workspace_root, &project.host)
            .await
            .map_err(|error| PullRequestError::from_provider("routeIdentity", error))?;
        if js_trim(&identity.viewer).is_empty() || js_trim(&identity.account_id).is_empty() {
            return Err(PullRequestError::operation("routeIdentity", "The signed-in account could not be verified."));
        }
        Ok(PullRequestRoutingResult {
            account_id: identity.account_id,
            host: project.host.clone(),
            provider: project.api.kind(),
            viewer: identity.viewer,
            project_title: project.project.title.clone(),
            workspace_root: project.project.workspace_root.clone(),
        })
    }
}
