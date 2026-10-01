//! The WS RPC handlers of this module (the `ws.ts` entries):
//!
//! | Method | Scope | Behaviour |
//! |---|---|---|
//! | `server.discoverSourceControl` | `orchestration:read` | [`SourceControlDiscovery::discover`] (never fails) |
//! | `sourceControl.lookupRepository` | `orchestration:read` | [`SourceControlRepositoryService::lookup_repository`] |
//! | `sourceControl.cloneRepository` | `orchestration:operate` | [`SourceControlRepositoryService::clone_repository`] (120 s deadline, no progress) |
//! | `sourceControl.publishRepository` | `orchestration:operate` | [`SourceControlRepositoryService::publish_repository`], then the [`AfterPublish`] hook |
//!
//! Failures are `SourceControlRepositoryError`s. Payloads are decoded with the zc-contracts
//! types; `TrimmedNonEmptyString` fields are trimmed and an empty required one is rejected like
//! the TS schema decode (a per-request `Die`).

use std::sync::Arc;

use futures::future::BoxFuture;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use zc_contracts::{
    Rpc, ServerDiscoverSourceControlPayload, SourceControlCloneRepositoryInput, SourceControlCloneRepositoryResult, SourceControlDiscoveryResult,
    SourceControlPublishRepositoryInput, SourceControlPublishRepositoryResult, SourceControlRepositoryInfo, SourceControlRepositoryLookupInput,
};
use zc_rpc::{Failure, MethodOptions, RpcMethod, RpcRouterBuilder, ScopeRule};

use crate::errors::SourceControlRepositoryError;
use crate::repository::{SourceControlCloneOptions, SourceControlRepositoryService};
use crate::source_control_discovery::SourceControlDiscovery;
use crate::util::js_trim;

/// After a successful publish: `ws.ts` refreshes the repository identity of `cwd` and its git
/// status (`repositoryIdentityResolver.resolve(cwd, {refresh: true})`, `refreshGitStatus(cwd)`).
pub type AfterPublish = Arc<dyn Fn(String) -> BoxFuture<'static, ()> + Send + Sync>;

/// What the handlers need.
#[derive(Clone)]
pub struct SourceControlRpcServices {
    pub discovery: SourceControlDiscovery,
    pub repositories: SourceControlRepositoryService,
    pub after_publish: Option<AfterPublish>,
}

/// A payload that cannot be read (`Never` as the success of a decode-only marker).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Never {}

macro_rules! method {
    ($name:ident, $rpc:expr, $payload:ty, $success:ty, $error:ty) => {
        #[doc = concat!("`", stringify!($name), "` as a typed zc-rpc method.")]
        pub struct $name;
        impl RpcMethod for $name {
            const TAG: &'static str = $rpc.tag();
            const STREAM: bool = false;
            type Payload = $payload;
            type Success = $success;
            type Error = $error;
        }
    };
}

method!(
    ServerDiscoverSourceControl,
    Rpc::ServerDiscoverSourceControl,
    ServerDiscoverSourceControlPayload,
    SourceControlDiscoveryResult,
    Never
);
method!(
    SourceControlLookupRepository,
    Rpc::SourceControlLookupRepository,
    SourceControlRepositoryLookupInput,
    SourceControlRepositoryInfo,
    SourceControlRepositoryError
);
method!(
    SourceControlCloneRepository,
    Rpc::SourceControlCloneRepository,
    SourceControlCloneRepositoryInput,
    SourceControlCloneRepositoryResult,
    SourceControlRepositoryError
);
method!(
    SourceControlPublishRepository,
    Rpc::SourceControlPublishRepository,
    SourceControlPublishRepositoryInput,
    SourceControlPublishRepositoryResult,
    SourceControlRepositoryError
);

fn options(rpc: Rpc) -> MethodOptions {
    MethodOptions::default().scope(ScopeRule::required(rpc.spec().scope.as_str()))
}

/// `TrimmedNonEmptyString` decoding of a required field.
fn required<E>(value: &mut String, field: &str) -> Result<(), Failure<E>> {
    let trimmed = js_trim(value).to_owned();
    if trimmed.is_empty() {
        return Err(Failure::Die(serde_json::Value::String(format!(
            "Expected a non empty string at [\"{field}\"], got {value:?}"
        ))));
    }
    *value = trimmed;
    Ok(())
}

/// The same for an optional field.
fn optional<E>(value: &mut Option<String>, field: &str) -> Result<(), Failure<E>> {
    match value {
        Some(inner) => required(inner, field),
        None => Ok(()),
    }
}

/// `server.discoverSourceControl`.
pub async fn discover_source_control(services: &SourceControlRpcServices) -> SourceControlDiscoveryResult {
    services.discovery.discover().await
}

/// `sourceControl.lookupRepository`.
pub async fn lookup_repository(
    services: &SourceControlRpcServices,
    mut input: SourceControlRepositoryLookupInput,
) -> Result<SourceControlRepositoryInfo, Failure<SourceControlRepositoryError>> {
    required(&mut input.repository, "repository")?;
    optional(&mut input.cwd, "cwd")?;
    Ok(services.repositories.lookup_repository(&input).await?)
}

/// `sourceControl.cloneRepository`.
pub async fn clone_repository(
    services: &SourceControlRpcServices,
    mut input: SourceControlCloneRepositoryInput,
) -> Result<SourceControlCloneRepositoryResult, Failure<SourceControlRepositoryError>> {
    required(&mut input.destination_path, "destinationPath")?;
    optional(&mut input.repository, "repository")?;
    optional(&mut input.remote_url, "remoteUrl")?;
    Ok(services.repositories.clone_repository(&input, &SourceControlCloneOptions::default()).await?)
}

/// `sourceControl.publishRepository`.
pub async fn publish_repository(
    services: &SourceControlRpcServices,
    mut input: SourceControlPublishRepositoryInput,
) -> Result<SourceControlPublishRepositoryResult, Failure<SourceControlRepositoryError>> {
    required(&mut input.cwd, "cwd")?;
    required(&mut input.repository, "repository")?;
    optional(&mut input.remote_name, "remoteName")?;
    let result = services.repositories.publish_repository(&input).await?;
    if let Some(after_publish) = &services.after_publish {
        after_publish(input.cwd.clone()).await;
    }
    Ok(result)
}

fn typed<M, F, Fut>(builder: RpcRouterBuilder, rpc: Rpc, services: &SourceControlRpcServices, handler: F) -> RpcRouterBuilder
where
    M: RpcMethod,
    M::Payload: DeserializeOwned,
    F: Fn(SourceControlRpcServices, M::Payload) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<M::Success, Failure<M::Error>>> + Send + 'static,
{
    let services = services.clone();
    builder.typed_unary_with::<M, _, _>(options(rpc), move |_ctx, payload| handler(services.clone(), payload))
}

/// Registers the four methods on a router builder.
pub fn register(builder: RpcRouterBuilder, services: SourceControlRpcServices) -> RpcRouterBuilder {
    let builder = typed::<ServerDiscoverSourceControl, _, _>(builder, Rpc::ServerDiscoverSourceControl, &services, |services, _payload| async move {
        Ok(discover_source_control(&services).await)
    });
    let builder = typed::<SourceControlLookupRepository, _, _>(builder, Rpc::SourceControlLookupRepository, &services, |services, payload| async move {
        lookup_repository(&services, payload).await
    });
    let builder = typed::<SourceControlCloneRepository, _, _>(builder, Rpc::SourceControlCloneRepository, &services, |services, payload| async move {
        clone_repository(&services, payload).await
    });
    typed::<SourceControlPublishRepository, _, _>(builder, Rpc::SourceControlPublishRepository, &services, |services, payload| async move {
        publish_repository(&services, payload).await
    })
}
