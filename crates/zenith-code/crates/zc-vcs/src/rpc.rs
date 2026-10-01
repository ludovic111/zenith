//! The WS RPC handlers of the VCS and review methods (the `ws.ts` entries, lines ~3286–3430),
//! over raw JSON as the client encodes it. Not registered in a server yet: [`register`] adds
//! them to a [`zc_rpc::RpcRouterBuilder`] with their scopes (`RpcAuthorization.ts`).
//!
//! | Method | Kind | Scope | Behaviour |
//! |---|---|---|---|
//! | `subscribeVcsStatus` | stream | `orchestration:read` | `VcsStatusBroadcaster.streamStatus(input, {automaticRemoteRefreshInterval})` |
//! | `vcs.refreshStatus` | unary | `orchestration:read` | `refreshStatus(cwd)` |
//! | `vcs.pull` | unary | `orchestration:operate` | `pullCurrentBranch(cwd)`, then a detached status refresh |
//! | `vcs.listRefs` | unary | `orchestration:read` | `listRefs(input)` |
//! | `vcs.createWorktree` / `removeWorktree` / `createRef` / `switchRef` | unary | `orchestration:operate` | workflow call, then a detached status refresh |
//! | `vcs.init` | unary | `orchestration:operate` | `VcsProvisioningService.initRepository`, then a refresh |
//! | `review.getDiffPreview` / `getDiffFileContents` | unary | `review:write` | `ReviewService` |
//!
//! A payload that does not decode dies with the decode error text (a per-request `Die`), like
//! the TS server's schema decode failure.

use futures::StreamExt;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use zc_ports::git::CreateWorktreeOptions;
use zc_rpc::{BoxValueStream, MethodOptions, RpcError, RpcRouterBuilder, ScopeRule};

use crate::broadcaster::{RefreshInterval, VcsStatusBroadcaster};
use crate::contracts::*;
use crate::registry::VcsProvisioningService;
use crate::review::ReviewService;
use crate::workflow::GitWorkflowService;

pub const SUBSCRIBE_VCS_STATUS: &str = "subscribeVcsStatus";
pub const VCS_REFRESH_STATUS: &str = "vcs.refreshStatus";
pub const VCS_PULL: &str = "vcs.pull";
pub const VCS_LIST_REFS: &str = "vcs.listRefs";
pub const VCS_CREATE_WORKTREE: &str = "vcs.createWorktree";
pub const VCS_REMOVE_WORKTREE: &str = "vcs.removeWorktree";
pub const VCS_CREATE_REF: &str = "vcs.createRef";
pub const VCS_SWITCH_REF: &str = "vcs.switchRef";
pub const VCS_INIT: &str = "vcs.init";
pub const REVIEW_GET_DIFF_PREVIEW: &str = "review.getDiffPreview";
pub const REVIEW_GET_DIFF_FILE_CONTENTS: &str = "review.getDiffFileContents";

const ORCHESTRATION_READ: &str = "orchestration:read";
const ORCHESTRATION_OPERATE: &str = "orchestration:operate";
const REVIEW_WRITE: &str = "review:write";

/// Every method of this module with its required scope.
pub const METHOD_SCOPES: [(&str, &str); 11] = [
    (SUBSCRIBE_VCS_STATUS, ORCHESTRATION_READ),
    (VCS_REFRESH_STATUS, ORCHESTRATION_READ),
    (VCS_PULL, ORCHESTRATION_OPERATE),
    (VCS_LIST_REFS, ORCHESTRATION_READ),
    (VCS_CREATE_WORKTREE, ORCHESTRATION_OPERATE),
    (VCS_REMOVE_WORKTREE, ORCHESTRATION_OPERATE),
    (VCS_CREATE_REF, ORCHESTRATION_OPERATE),
    (VCS_SWITCH_REF, ORCHESTRATION_OPERATE),
    (VCS_INIT, ORCHESTRATION_OPERATE),
    (REVIEW_GET_DIFF_PREVIEW, REVIEW_WRITE),
    (REVIEW_GET_DIFF_FILE_CONTENTS, REVIEW_WRITE),
];

/// What the handlers need.
#[derive(Clone)]
pub struct VcsRpcServices {
    pub workflow: GitWorkflowService,
    pub broadcaster: VcsStatusBroadcaster,
    pub provisioning: VcsProvisioningService,
    pub review: ReviewService,
    /// `automaticGitFetchInterval` from the server settings.
    pub automatic_git_fetch_interval: RefreshInterval,
}

fn decode<T: DeserializeOwned>(payload: Value) -> Result<T, RpcError> {
    serde_json::from_value(payload).map_err(|error| RpcError::die_text(error.to_string()))
}

fn encode<T: Serialize>(value: &T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|e| RpcError::die(format!("could not encode the result: {e}")))
}

/// `refreshGitStatus(cwd)`: a detached `refreshStatus`, failures only logged.
pub fn spawn_status_refresh(broadcaster: &VcsStatusBroadcaster, cwd: &str) {
    let broadcaster = broadcaster.clone();
    let cwd = cwd.to_owned();
    tokio::spawn(async move {
        if let Err(error) = broadcaster.refresh_status(&cwd).await {
            tracing::info!(cwd, error = %error, "VCS status refresh after an RPC failed");
        }
    });
}

/// `subscribeVcsStatus`.
pub async fn subscribe_vcs_status(services: &VcsRpcServices, payload: Value) -> Result<BoxValueStream, RpcError> {
    let input: VcsStatusInput = decode(payload)?;
    let stream = services
        .broadcaster
        .stream_status(&input.cwd, Some(services.automatic_git_fetch_interval.clone()))
        .await
        .map_err(RpcError::fail)?;
    Ok(Box::pin(stream.map(|event| encode(&event))))
}

/// `vcs.refreshStatus`.
pub async fn vcs_refresh_status(services: &VcsRpcServices, payload: Value) -> Result<Value, RpcError> {
    let input: VcsStatusInput = decode(payload)?;
    let status = services.broadcaster.refresh_status(&input.cwd).await.map_err(RpcError::fail)?;
    encode(&status)
}

/// `vcs.pull`.
pub async fn vcs_pull(services: &VcsRpcServices, payload: Value) -> Result<Value, RpcError> {
    let input: VcsPullInput = decode(payload)?;
    let result = services.workflow.pull_current_branch(&input.cwd).await.map_err(RpcError::fail)?;
    spawn_status_refresh(&services.broadcaster, &input.cwd);
    encode(&result)
}

/// `vcs.listRefs`.
pub async fn vcs_list_refs(services: &VcsRpcServices, payload: Value) -> Result<Value, RpcError> {
    let input: VcsListRefsInput = decode(payload)?;
    input.validate().map_err(RpcError::die_text)?;
    let result = services.workflow.list_refs(&input).await.map_err(RpcError::fail)?;
    encode(&result)
}

/// `vcs.createWorktree`.
pub async fn vcs_create_worktree(services: &VcsRpcServices, payload: Value) -> Result<Value, RpcError> {
    let input: VcsCreateWorktreeInput = decode(payload)?;
    let result = services
        .workflow
        .create_worktree(&input, &CreateWorktreeOptions::default())
        .await
        .map_err(RpcError::fail)?;
    spawn_status_refresh(&services.broadcaster, &input.cwd);
    encode(&result)
}

/// `vcs.removeWorktree` (void success).
pub async fn vcs_remove_worktree(services: &VcsRpcServices, payload: Value) -> Result<Value, RpcError> {
    let input: VcsRemoveWorktreeInput = decode(payload)?;
    services.workflow.remove_worktree(&input).await.map_err(RpcError::fail)?;
    spawn_status_refresh(&services.broadcaster, &input.cwd);
    Ok(Value::Null)
}

/// `vcs.createRef`.
pub async fn vcs_create_ref(services: &VcsRpcServices, payload: Value) -> Result<Value, RpcError> {
    let input: VcsCreateRefInput = decode(payload)?;
    let result = services.workflow.create_ref(&input).await.map_err(RpcError::fail)?;
    spawn_status_refresh(&services.broadcaster, &input.cwd);
    encode(&result)
}

/// `vcs.switchRef`.
pub async fn vcs_switch_ref(services: &VcsRpcServices, payload: Value) -> Result<Value, RpcError> {
    let input: VcsSwitchRefInput = decode(payload)?;
    let result = services.workflow.switch_ref(&input).await.map_err(RpcError::fail)?;
    spawn_status_refresh(&services.broadcaster, &input.cwd);
    encode(&result)
}

/// `vcs.init` (void success).
pub async fn vcs_init(services: &VcsRpcServices, payload: Value) -> Result<Value, RpcError> {
    let input: VcsInitInput = decode(payload)?;
    services.provisioning.init_repository(&input).await.map_err(RpcError::fail)?;
    spawn_status_refresh(&services.broadcaster, &input.cwd);
    Ok(Value::Null)
}

/// `review.getDiffPreview`.
pub async fn review_get_diff_preview(services: &VcsRpcServices, payload: Value) -> Result<Value, RpcError> {
    let input: ReviewDiffPreviewInput = decode(payload)?;
    let result = services.review.get_diff_preview(&input).await.map_err(RpcError::fail)?;
    encode(&result)
}

/// `review.getDiffFileContents`.
pub async fn review_get_diff_file_contents(services: &VcsRpcServices, payload: Value) -> Result<Value, RpcError> {
    let input: ReviewDiffFileContentsInput = decode(payload)?;
    let result = services.review.get_diff_file_contents(&input).await.map_err(RpcError::fail)?;
    encode(&result)
}

macro_rules! unary {
    ($builder:expr, $services:expr, $tag:expr, $scope:expr, $handler:path) => {{
        let services = $services.clone();
        $builder.unary_with($tag, MethodOptions::default().scope(ScopeRule::required($scope)), move |_ctx, payload| {
            let services = services.clone();
            async move { $handler(&services, payload).await }
        })
    }};
}

/// Register every handler of this module on a router builder.
pub fn register(builder: RpcRouterBuilder, services: VcsRpcServices) -> RpcRouterBuilder {
    let stream_services = services.clone();
    let builder = builder.stream_with(
        SUBSCRIBE_VCS_STATUS,
        MethodOptions::default().scope(ScopeRule::required(ORCHESTRATION_READ)),
        move |_ctx, payload| {
            let services = stream_services.clone();
            async move { subscribe_vcs_status(&services, payload).await }
        },
    );
    let builder = unary!(builder, services, VCS_REFRESH_STATUS, ORCHESTRATION_READ, vcs_refresh_status);
    let builder = unary!(builder, services, VCS_PULL, ORCHESTRATION_OPERATE, vcs_pull);
    let builder = unary!(builder, services, VCS_LIST_REFS, ORCHESTRATION_READ, vcs_list_refs);
    let builder = unary!(builder, services, VCS_CREATE_WORKTREE, ORCHESTRATION_OPERATE, vcs_create_worktree);
    let builder = unary!(builder, services, VCS_REMOVE_WORKTREE, ORCHESTRATION_OPERATE, vcs_remove_worktree);
    let builder = unary!(builder, services, VCS_CREATE_REF, ORCHESTRATION_OPERATE, vcs_create_ref);
    let builder = unary!(builder, services, VCS_SWITCH_REF, ORCHESTRATION_OPERATE, vcs_switch_ref);
    let builder = unary!(builder, services, VCS_INIT, ORCHESTRATION_OPERATE, vcs_init);
    let builder = unary!(builder, services, REVIEW_GET_DIFF_PREVIEW, REVIEW_WRITE, review_get_diff_preview);
    unary!(builder, services, REVIEW_GET_DIFF_FILE_CONTENTS, REVIEW_WRITE, review_get_diff_file_contents)
}
