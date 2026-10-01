//! The WS RPC handlers of this crate (`ws.ts`), with their scopes (`RpcAuthorization.ts`):
//!
//! | Method | Kind | Scope | Behaviour |
//! |---|---|---|---|
//! | `projectClone.start` | unary | `orchestration:operate` | [`ProjectCloneTracker::start`] with the caller's [`ProjectCloneHooks`]; fails with `SourceControlRepositoryError` or `OrchestrationDispatchCommandError` |
//! | `projectClone.cancel` | unary | `orchestration:operate` | [`ProjectCloneTracker::cancel`] → `{applied}` |
//! | `projectClone.retry` | unary | `orchestration:operate` | [`ProjectCloneTracker::retry`] → `{applied}`; `SourceControlRepositoryError` |
//! | `subscribeProjectClones` | stream | `orchestration:read` | [`ProjectCloneTracker::stream`]: every list of tracked clones |
//! | `agentSessions.scan` | unary | `orchestration:read` | [`AgentSessionScanner::scan`]; `AgentSessionScanError` |
//! | `agentSessions.import` | unary | `orchestration:operate` | [`AgentSessionImporter::import`]; `AgentSessionImport*Error`, `AgentSessionScanError` |
//!
//! Payloads decode with the zc-contracts types; `TrimmedNonEmptyString` fields are trimmed and
//! an empty one dies with the decode message, like the TS schema decode.

use std::sync::Arc;

use futures::StreamExt;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use zc_contracts::{AgentSessionImportInput, ProjectCloneActionInput, ProjectCloneActionResult, ProjectCloneStartInput};
use zc_rpc::{MethodOptions, RequestContext, RpcError, RpcRouterBuilder, ScopeRule};

use crate::clone::{ProjectCloneHooks, ProjectCloneTracker};
use crate::sessions::{AgentSessionImporter, AgentSessionScanner};

pub const PROJECT_CLONE_START: &str = "projectClone.start";
pub const PROJECT_CLONE_CANCEL: &str = "projectClone.cancel";
pub const PROJECT_CLONE_RETRY: &str = "projectClone.retry";
pub const SUBSCRIBE_PROJECT_CLONES: &str = "subscribeProjectClones";
pub const AGENT_SESSIONS_SCAN: &str = "agentSessions.scan";
pub const AGENT_SESSIONS_IMPORT: &str = "agentSessions.import";

const ORCHESTRATION_READ: &str = "orchestration:read";
const ORCHESTRATION_OPERATE: &str = "orchestration:operate";

/// Every method of this module with its required scope.
pub const METHOD_SCOPES: [(&str, &str); 6] = [
    (PROJECT_CLONE_START, ORCHESTRATION_OPERATE),
    (PROJECT_CLONE_CANCEL, ORCHESTRATION_OPERATE),
    (PROJECT_CLONE_RETRY, ORCHESTRATION_OPERATE),
    (SUBSCRIBE_PROJECT_CLONES, ORCHESTRATION_READ),
    (AGENT_SESSIONS_SCAN, ORCHESTRATION_READ),
    (AGENT_SESSIONS_IMPORT, ORCHESTRATION_OPERATE),
];

/// The clone hooks of one request (they dispatch with the connection's origin).
pub type CloneHooksFactory = Arc<dyn Fn(&RequestContext) -> Arc<dyn ProjectCloneHooks> + Send + Sync>;

/// What the handlers need.
#[derive(Clone)]
pub struct ProjectRpcServices {
    pub clones: ProjectCloneTracker,
    pub clone_hooks: CloneHooksFactory,
    pub scanner: AgentSessionScanner,
    pub importer: AgentSessionImporter,
}

fn decode<T: DeserializeOwned>(payload: Value) -> Result<T, RpcError> {
    serde_json::from_value(payload).map_err(|error| RpcError::die_text(error.to_string()))
}

fn encode<T: Serialize>(value: &T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|e| RpcError::die(format!("could not encode the result: {e}")))
}

fn options(scope: &str) -> MethodOptions {
    MethodOptions::default().scope(ScopeRule::required(scope))
}

/// `TrimmedNonEmptyString`: trim, and refuse an empty value.
fn trimmed(value: &mut String, field: &str) -> Result<(), RpcError> {
    let trimmed = value.trim().to_owned();
    if trimmed.is_empty() {
        return Err(RpcError::die_text(format!("Expected a non empty string at [\"{field}\"], got {value:?}")));
    }
    *value = trimmed;
    Ok(())
}

fn trimmed_optional(value: &mut Option<String>, field: &str) -> Result<(), RpcError> {
    match value {
        Some(inner) => trimmed(inner, field),
        None => Ok(()),
    }
}

/// `projectClone.start`.
pub async fn project_clone_start(services: &ProjectRpcServices, ctx: &RequestContext, payload: Value) -> Result<Value, RpcError> {
    let mut input: ProjectCloneStartInput = decode(payload)?;
    trimmed(&mut input.title, "title")?;
    trimmed(&mut input.destination_path, "destinationPath")?;
    trimmed_optional(&mut input.repository, "repository")?;
    trimmed_optional(&mut input.remote_url, "remoteUrl")?;
    let hooks = (services.clone_hooks)(ctx);
    match services.clones.start(input, hooks).await {
        Ok(result) => encode(&result),
        Err(error) => Err(RpcError::Fail(error.to_wire())),
    }
}

/// `projectClone.cancel`.
pub async fn project_clone_cancel(services: &ProjectRpcServices, payload: Value) -> Result<Value, RpcError> {
    let input: ProjectCloneActionInput = decode(payload)?;
    let applied = services.clones.cancel(&input.project_id).await;
    encode(&ProjectCloneActionResult { applied })
}

/// `projectClone.retry`.
pub async fn project_clone_retry(services: &ProjectRpcServices, payload: Value) -> Result<Value, RpcError> {
    let input: ProjectCloneActionInput = decode(payload)?;
    match services.clones.retry(&input.project_id).await {
        Ok(applied) => encode(&ProjectCloneActionResult { applied }),
        Err(error) => Err(RpcError::fail(error.to_wire())),
    }
}

/// `agentSessions.scan`.
pub async fn agent_sessions_scan(services: &ProjectRpcServices) -> Result<Value, RpcError> {
    match services.scanner.scan().await {
        Ok(result) => encode(&result),
        Err(error) => Err(RpcError::fail(error)),
    }
}

/// `agentSessions.import`.
pub async fn agent_sessions_import(services: &ProjectRpcServices, payload: Value) -> Result<Value, RpcError> {
    let mut input: AgentSessionImportInput = decode(payload)?;
    trimmed_optional(&mut input.expected_workspace_root, "expectedWorkspaceRoot")?;
    match services.importer.import(input).await {
        Ok(result) => encode(&result),
        Err(error) => Err(RpcError::fail(error)),
    }
}

/// Adds every method of the table above.
pub fn register(builder: RpcRouterBuilder, services: ProjectRpcServices) -> RpcRouterBuilder {
    let s = services.clone();
    let builder = builder.unary_with(PROJECT_CLONE_START, options(ORCHESTRATION_OPERATE), move |ctx, payload| {
        let s = s.clone();
        async move { project_clone_start(&s, &ctx, payload).await }
    });
    let s = services.clone();
    let builder = builder.unary_with(PROJECT_CLONE_CANCEL, options(ORCHESTRATION_OPERATE), move |_ctx, payload| {
        let s = s.clone();
        async move { project_clone_cancel(&s, payload).await }
    });
    let s = services.clone();
    let builder = builder.unary_with(PROJECT_CLONE_RETRY, options(ORCHESTRATION_OPERATE), move |_ctx, payload| {
        let s = s.clone();
        async move { project_clone_retry(&s, payload).await }
    });
    let s = services.clone();
    let builder = builder.stream_with(SUBSCRIBE_PROJECT_CLONES, options(ORCHESTRATION_READ), move |_ctx, _payload| {
        let s = s.clone();
        async move { Ok(s.clones.stream().map(|list| encode(&list))) }
    });
    let s = services.clone();
    let builder = builder.unary_with(AGENT_SESSIONS_SCAN, options(ORCHESTRATION_READ), move |_ctx, _payload| {
        let s = s.clone();
        async move { agent_sessions_scan(&s).await }
    });
    let s = services;
    builder.unary_with(AGENT_SESSIONS_IMPORT, options(ORCHESTRATION_OPERATE), move |_ctx, payload| {
        let s = s.clone();
        async move { agent_sessions_import(&s, payload).await }
    })
}
