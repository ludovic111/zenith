//! The `git.*` WS RPC handlers of `ws.ts`.
//!
//! | Method | Kind | Scope | Behaviour |
//! |---|---|---|---|
//! | `git.runStackedAction` | stream | `orchestration:operate` | the action's progress events; on success the created PR is linked to `threadId` and the status refreshed, then the stream ends; a failure ends it with the `GitManagerServiceError` |
//! | `git.resolvePullRequest` | unary | `orchestration:operate` | `resolvePullRequest(input)` |
//! | `git.preparePullRequestThread` | unary | `orchestration:operate` | `preparePullRequestThread(input)`, then a detached status refresh |
//!
//! Interrupting the stream (the client going away) cancels the action, like the TS fiber.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::Stream;
use serde_json::Value;
use zc_ports::git::GitRunStackedActionOptions;
use zc_ports::{OrchestrationDispatch, ProjectionReads};
use zc_rpc::{BoxValueStream, MethodOptions, RpcError, RpcRouterBuilder, ScopeRule};
use zc_vcs::errors::IntoTagged;
use zc_vcs::rpc::spawn_status_refresh;
use zc_vcs::{GitWorkflowService, VcsStatusBroadcaster};

use crate::link::link_created_pull_request;
use crate::manager::UuidSource;
use crate::types::{GitPreparePullRequestThreadInput, GitPullRequestRefInput, GitRunStackedActionInput, GitRunStackedActionResult};

pub const GIT_RUN_STACKED_ACTION: &str = "git.runStackedAction";
pub const GIT_RESOLVE_PULL_REQUEST: &str = "git.resolvePullRequest";
pub const GIT_PREPARE_PULL_REQUEST_THREAD: &str = "git.preparePullRequestThread";

const ORCHESTRATION_OPERATE: &str = "orchestration:operate";

/// What the handlers need.
#[derive(Clone)]
pub struct GitRpcServices {
    pub workflow: GitWorkflowService,
    pub broadcaster: VcsStatusBroadcaster,
    pub engine: Arc<dyn OrchestrationDispatch>,
    pub projections: Arc<dyn ProjectionReads>,
    pub uuids: UuidSource,
}

fn decode<T: serde::de::DeserializeOwned>(payload: Value) -> Result<T, RpcError> {
    serde_json::from_value(payload).map_err(|error| RpcError::die_text(error.to_string()))
}

/// The receiving end of a running action: dropping it cancels the action.
struct ActionStream {
    events: tokio::sync::mpsc::UnboundedReceiver<Result<Value, RpcError>>,
    task: tokio::task::AbortHandle,
}

impl Stream for ActionStream {
    type Item = Result<Value, RpcError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.events.poll_recv(cx)
    }
}

impl Drop for ActionStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// `git.runStackedAction`.
pub async fn run_stacked_action(services: &GitRpcServices, payload: Value) -> Result<BoxValueStream, RpcError> {
    let input: GitRunStackedActionInput = decode(payload.clone())?;
    let (sender, events) = tokio::sync::mpsc::unbounded_channel::<Result<Value, RpcError>>();
    let reporter_sender = sender.clone();
    let options = GitRunStackedActionOptions {
        action_id: Some(input.action_id.clone()),
        progress_reporter: Some(Arc::new(move |event: zc_ports::contracts::GitActionProgressEvent| {
            let _ = reporter_sender.send(Ok(event.0));
        })),
    };
    let services = services.clone();
    let encoded = serde_json::to_value(&input).unwrap_or(payload);
    let task = tokio::spawn(async move {
        match services.workflow.run_stacked_action(encoded, options).await {
            Ok(result) => {
                if let Some(thread_id) = input.thread_id.as_deref() {
                    if let Ok(result) = serde_json::from_value::<GitRunStackedActionResult>(result) {
                        let command_id = format!("server:pr-created-link:{}", (services.uuids)());
                        link_created_pull_request(&*services.engine, &*services.projections, thread_id, &result.pr, command_id).await;
                    }
                }
                spawn_status_refresh(&services.broadcaster, &input.cwd);
            }
            Err(error) => {
                let _ = sender.send(Err(RpcError::fail(error.into_tagged())));
            }
        }
    });
    Ok(Box::pin(ActionStream {
        events,
        task: task.abort_handle(),
    }))
}

/// `git.resolvePullRequest`.
pub async fn resolve_pull_request(services: &GitRpcServices, payload: Value) -> Result<Value, RpcError> {
    let input: GitPullRequestRefInput = decode(payload)?;
    services
        .workflow
        .resolve_pull_request(serde_json::to_value(&input).unwrap_or(Value::Null))
        .await
        .map_err(|error| RpcError::fail(error.into_tagged()))
}

/// `git.preparePullRequestThread`.
pub async fn prepare_pull_request_thread(services: &GitRpcServices, payload: Value) -> Result<Value, RpcError> {
    let input: GitPreparePullRequestThreadInput = decode(payload)?;
    let result = services
        .workflow
        .prepare_pull_request_thread(serde_json::to_value(&input).unwrap_or(Value::Null))
        .await
        .map_err(|error| RpcError::fail(error.into_tagged()))?;
    spawn_status_refresh(&services.broadcaster, &input.cwd);
    Ok(result)
}

/// Register the `git.*` handlers.
pub fn register(builder: RpcRouterBuilder, services: GitRpcServices) -> RpcRouterBuilder {
    let operate = || MethodOptions::default().scope(ScopeRule::required(ORCHESTRATION_OPERATE));
    let stream_services = services.clone();
    let builder = builder.stream_with(GIT_RUN_STACKED_ACTION, operate(), move |_ctx, payload| {
        let services = stream_services.clone();
        async move { run_stacked_action(&services, payload).await }
    });
    let resolve_services = services.clone();
    let builder = builder.unary_with(GIT_RESOLVE_PULL_REQUEST, operate(), move |_ctx, payload| {
        let services = resolve_services.clone();
        async move { resolve_pull_request(&services, payload).await }
    });
    builder.unary_with(GIT_PREPARE_PULL_REQUEST_THREAD, operate(), move |_ctx, payload| {
        let services = services.clone();
        async move { prepare_pull_request_thread(&services, payload).await }
    })
}
