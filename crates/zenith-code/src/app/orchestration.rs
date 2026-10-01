//! Client command dispatch (`ws.ts` `orchestration.dispatchCommand`, `orchestration/http.ts`
//! `dispatch`): normalize the client command (server timestamps, workspace roots,
//! attachments), dispatch it through the engine with the connection's origin, clean up
//! claimed uploads on failure, and run the archive side effects (stop the session, close the
//! thread's terminals).
//!
//! Packages that hook into dispatch implement [`DispatchHook`] (registered from
//! `app::plugins`): the worktree bootstrap of `thread.turn.start` (WP-11), the deletion
//! reactor's drain after `thread.create` (WP-10), the clone tracker's guard (WP-25).

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use serde_json::{json, Value};
use zc_contracts::{
    ClientOrchestrationCommand, LitOrchestrationDispatchCommandError, OrchestrationClientOrigin, OrchestrationCommand, OrchestrationDispatchCommandError,
    ThreadId,
};
use zc_orchestration::normalizer::{cleanup_failed_uploaded_attachments, dispatch_command_error, normalize_dispatch_command};
use zc_orchestration::OrchestrationEngine;
use zc_ports::{DispatchResult, ProjectionReads, TerminalManager};
use zc_rpc::{RequestContext, RpcError, RpcRouterBuilder};

/// What a dispatch hook may answer instead of the engine.
pub type HookResult = Result<DispatchResult, OrchestrationDispatchCommandError>;

/// A package's part in client dispatch. Every method has a no-op default.
#[async_trait]
pub trait DispatchHook: Send + Sync {
    /// Before normalization: refuse a command (`ProjectCloneTracker.rejectCommandsDuringClone`).
    async fn before(&self, _command: &ClientOrchestrationCommand) -> Result<(), OrchestrationDispatchCommandError> {
        Ok(())
    }

    /// Take over a normalized command (`dispatchBootstrapTurnStart` for a `thread.turn.start`
    /// with a `bootstrap`). `None` lets the next hook or the engine handle it.
    async fn dispatch(&self, _command: &OrchestrationCommand, _origin: Option<&OrchestrationClientOrigin>) -> Option<HookResult> {
        None
    }

    /// After a successful dispatch (`threadDeletionReactor.drainThrough(sequence)` after
    /// `thread.create`, `discardCloneForDeletedProject`).
    async fn after(&self, _command: &OrchestrationCommand, _result: &DispatchResult) {}
}

/// Why a client dispatch failed.
#[derive(Debug)]
pub enum DispatchFailure {
    /// The command did not normalize, or a hook refused it.
    Invalid(OrchestrationDispatchCommandError),
    /// The engine (or a hook that took the command over) failed.
    Failed(OrchestrationDispatchCommandError),
}

impl DispatchFailure {
    pub fn into_error(self) -> OrchestrationDispatchCommandError {
        match self {
            Self::Invalid(error) | Self::Failed(error) => error,
        }
    }
}

/// `toDispatchCommandError(cause, message)`: the cause encoded as a defect (`{name, message}`).
pub fn dispatch_failed(message: &str, cause_tag: &str, cause_message: &str) -> OrchestrationDispatchCommandError {
    OrchestrationDispatchCommandError {
        tag: LitOrchestrationDispatchCommandError,
        message: message.into(),
        cause: Some(json!({ "name": cause_tag, "message": cause_message })),
        bootstrap_thread_disposition: None,
    }
}

/// The dispatcher shared by the WS RPC and the HTTP route.
pub struct ClientDispatcher {
    engine: OrchestrationEngine,
    reads: Arc<dyn ProjectionReads>,
    terminals: Arc<dyn TerminalManager>,
    attachments_dir: PathBuf,
    hooks: RwLock<Vec<Arc<dyn DispatchHook>>>,
}

impl ClientDispatcher {
    pub fn new(engine: OrchestrationEngine, reads: Arc<dyn ProjectionReads>, terminals: Arc<dyn TerminalManager>, attachments_dir: PathBuf) -> Self {
        Self {
            engine,
            reads,
            terminals,
            attachments_dir,
            hooks: RwLock::new(Vec::new()),
        }
    }

    pub fn add_hook(&self, hook: Arc<dyn DispatchHook>) {
        self.hooks.write().unwrap_or_else(|p| p.into_inner()).push(hook);
    }

    fn hooks(&self) -> Vec<Arc<dyn DispatchHook>> {
        self.hooks.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Normalize and dispatch a client command.
    pub async fn dispatch(&self, command: ClientOrchestrationCommand, origin: Option<OrchestrationClientOrigin>) -> Result<DispatchResult, DispatchFailure> {
        let hooks = self.hooks();
        for hook in &hooks {
            hook.before(&command).await.map_err(DispatchFailure::Failed)?;
        }
        let attachments_dir = self.attachments_dir.clone();
        let received_at = zc_core::now_iso();
        let original = command.clone();
        let normalized = tokio::task::spawn_blocking(move || normalize_dispatch_command(command, &received_at, &attachments_dir))
            .await
            .map_err(|error| DispatchFailure::Failed(dispatch_command_error(format!("Failed to normalize the command: {error}"))))?
            .map_err(DispatchFailure::Invalid)?;
        let result = self.dispatch_normalized(&normalized, origin).await;
        match result {
            Ok(result) => {
                for hook in &hooks {
                    hook.after(&normalized, &result).await;
                }
                Ok(result)
            }
            Err(error) => {
                let attachments_dir = self.attachments_dir.clone();
                let _ = tokio::task::spawn_blocking(move || cleanup_failed_uploaded_attachments(&original, &normalized, &attachments_dir)).await;
                Err(DispatchFailure::Failed(error))
            }
        }
    }

    async fn dispatch_normalized(&self, command: &OrchestrationCommand, origin: Option<OrchestrationClientOrigin>) -> HookResult {
        for hook in self.hooks() {
            if let Some(result) = hook.dispatch(command, origin.as_ref()).await {
                return result;
            }
        }
        if let OrchestrationCommand::ThreadTurnStartCommand(turn) = command {
            if turn.bootstrap.is_some() {
                return Err(dispatch_command_error(
                    "Starting a thread in a new worktree is not implemented by the Rust server yet.",
                ));
            }
        }
        self.engine.dispatch(command.clone(), origin).await.map_err(|error| {
            let tagged = error.to_tagged();
            dispatch_failed("Failed to dispatch orchestration command", &tagged.tag, &error.to_string())
        })
    }

    /// Whether the thread has a provider session that is not stopped (read before archiving).
    async fn has_live_session(&self, thread_id: &ThreadId) -> bool {
        match self.reads.get_thread_shell_by_id(thread_id).await {
            Ok(Some(thread)) => thread.session.as_ref().is_some_and(|session| session.status.as_str() != "stopped"),
            Ok(None) => false,
            Err(error) => {
                tracing::warn!(thread_id = %thread_id, %error, "failed to read thread session state before session-stop check");
                false
            }
        }
    }

    /// `orchestration.dispatchCommand`: dispatch, then the archive side effects.
    pub async fn dispatch_from_socket(
        &self,
        command: ClientOrchestrationCommand,
        origin: Option<OrchestrationClientOrigin>,
    ) -> Result<DispatchResult, OrchestrationDispatchCommandError> {
        let archived = match &command {
            ClientOrchestrationCommand::ThreadArchive(archive) => Some((archive.thread_id.clone(), archive.command_id.to_string())),
            _ => None,
        };
        let stop_session = match &archived {
            Some((thread_id, _)) => self.has_live_session(thread_id).await,
            None => false,
        };
        let result = self.dispatch(command, origin.clone()).await.map_err(DispatchFailure::into_error)?;
        if let Some((thread_id, command_id)) = archived {
            if stop_session {
                let stop: Result<ClientOrchestrationCommand, _> = serde_json::from_value(json!({
                    "type": "thread.session.stop",
                    "commandId": format!("session-stop-for-archive:{command_id}"),
                    "threadId": thread_id,
                    "createdAt": zc_core::now_iso(),
                }));
                let stopped = match stop {
                    Ok(stop) => self.dispatch(stop, origin).await.map(|_| ()).map_err(|e| e.into_error().message.to_string()),
                    Err(error) => Err(error.to_string()),
                };
                if let Err(cause) = stopped {
                    tracing::warn!(thread_id = %thread_id, cause, "failed to stop provider session during archive");
                }
            }
            let close = zc_ports::contracts::TerminalCloseInput(json!({ "threadId": thread_id }));
            if let Err(error) = self.terminals.close(close).await {
                tracing::warn!(thread_id = %thread_id, ?error, "failed to close thread terminals after archive");
            }
        }
        Ok(result)
    }
}

/// `readClientConnectionOrigin`: the validated `clientSurface` / `clientAppVersion` of the
/// socket, `None` when it has neither.
pub fn connection_origin(ctx: &RequestContext) -> Option<OrchestrationClientOrigin> {
    let metadata = &ctx.connection.metadata;
    let query = zc_http::WsQuery {
        client_surface: metadata.get("clientSurface").cloned(),
        client_app_version: metadata.get("clientAppVersion").cloned(),
        ..zc_http::WsQuery::default()
    };
    let origin = query.client_origin();
    if origin.surface.is_none() && origin.app_version.is_none() {
        return None;
    }
    let mut value = serde_json::Map::new();
    if let Some(surface) = origin.surface {
        value.insert("surface".into(), Value::String(surface));
    }
    if let Some(version) = origin.app_version {
        value.insert("appVersion".into(), Value::String(version));
    }
    serde_json::from_value(Value::Object(value)).ok()
}

/// Registers `orchestration.dispatchCommand`.
pub fn register(builder: RpcRouterBuilder, dispatcher: Arc<ClientDispatcher>) -> RpcRouterBuilder {
    builder.unary("orchestration.dispatchCommand", move |ctx, payload| {
        let dispatcher = dispatcher.clone();
        async move {
            let command: ClientOrchestrationCommand = serde_json::from_value(payload).map_err(|error| RpcError::die_text(error.to_string()))?;
            let origin = connection_origin(&ctx);
            match dispatcher.dispatch_from_socket(command, origin).await {
                Ok(result) => Ok(json!({ "sequence": result.sequence })),
                Err(error) => Err(RpcError::fail(error)),
            }
        }
    })
}

/// [`zc_projections::http::HttpDispatch`] over the dispatcher (`POST /api/orchestration/dispatch`).
pub struct HttpDispatcher(pub Arc<ClientDispatcher>);

#[async_trait]
impl zc_projections::http::HttpDispatch for HttpDispatcher {
    async fn dispatch(&self, payload: Value) -> Result<DispatchResult, zc_projections::http::HttpDispatchError> {
        use zc_projections::http::HttpDispatchError;
        let command: ClientOrchestrationCommand = serde_json::from_value(payload).map_err(|_| HttpDispatchError::InvalidCommand)?;
        match self.0.dispatch(command, None).await {
            Ok(result) => Ok(result),
            Err(DispatchFailure::Invalid(error)) => {
                tracing::debug!(message = %error.message, "dispatch refused");
                Err(HttpDispatchError::InvalidCommand)
            }
            Err(DispatchFailure::Failed(error)) => Err(HttpDispatchError::Failed(error.message.to_string())),
        }
    }
}
