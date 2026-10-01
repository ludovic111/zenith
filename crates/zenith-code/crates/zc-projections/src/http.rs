//! `orchestration/http.ts`: the orchestration group of the environment HTTP API.
//!
//! | Route | Scope | Answer |
//! |---|---|---|
//! | `GET /api/orchestration/snapshot` | `orchestration:read` | the command read model (thread bodies empty; full hydration has OOM-killed servers) |
//! | `GET /api/orchestration/shell` | `orchestration:read` | the shell snapshot |
//! | `GET /api/orchestration/threads/{threadId}?reasoningMessages=true&turnLimit=N&beforeCursor=…` | `orchestration:read` | the projected thread detail snapshot, or 404 `thread_not_found` |
//! | `POST /api/orchestration/dispatch` | `orchestration:operate` | `{sequence}` |
//!
//! Authentication (the `EnvironmentAuthenticatedAuth` middleware) and dispatch normalization
//! belong to other crates; they come in through [`EnvironmentAuth`] and [`HttpDispatch`].

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use zc_contracts::{OrchestrationThreadDetailWindow, ThreadId};
use zc_http::EnvironmentError;
use zc_ports::{DispatchResult, ProjectionReads};

use crate::activity_payload::project_thread_detail_snapshot;

pub const ORCHESTRATION_READ_SCOPE: &str = "orchestration:read";
pub const ORCHESTRATION_OPERATE_SCOPE: &str = "orchestration:operate";

/// `EnvironmentAuthenticatedAuth`: the principal's scopes, or the 401 to answer.
#[async_trait]
pub trait EnvironmentAuth: Send + Sync {
    async fn authenticate(&self, parts: &Parts) -> Result<Vec<String>, EnvironmentError>;
}

/// Why a dispatch failed, as the route answers it.
#[derive(Debug, Clone, PartialEq)]
pub enum HttpDispatchError {
    /// The command does not normalize (`invalid_command`, 400).
    InvalidCommand,
    /// Anything else (`orchestration_dispatch_failed`, 500).
    Failed(String),
}

/// The dispatch side of `POST /api/orchestration/dispatch`: reject during a project clone,
/// normalize the client command (attachments), dispatch, clean up failed uploads, discard the
/// clone of a deleted project. Owned by the orchestration engine (WP-08) and the project crate.
#[async_trait]
pub trait HttpDispatch: Send + Sync {
    /// `payload` is an encoded `ClientOrchestrationCommand` (already validated by the route).
    async fn dispatch(&self, payload: Value) -> Result<DispatchResult, HttpDispatchError>;
}

#[derive(Clone)]
struct RouteState {
    reads: Arc<dyn ProjectionReads>,
    dispatch: Arc<dyn HttpDispatch>,
    auth: Arc<dyn EnvironmentAuth>,
}

/// The four routes, ready to merge into the server's router.
pub fn router(reads: Arc<dyn ProjectionReads>, dispatch: Arc<dyn HttpDispatch>, auth: Arc<dyn EnvironmentAuth>) -> Router {
    Router::new()
        .route("/api/orchestration/snapshot", get(snapshot))
        .route("/api/orchestration/shell", get(shell_snapshot))
        .route("/api/orchestration/threads/{thread_id}", get(thread_snapshot))
        .route("/api/orchestration/dispatch", post(dispatch_command))
        .with_state(RouteState { reads, dispatch, auth })
}

/// `requireEnvironmentScope` on an authenticated principal's scopes.
fn check_scope(scopes: &[String], scope: &str) -> Result<(), EnvironmentError> {
    if scopes.iter().any(|granted| granted == scope) {
        Ok(())
    } else {
        Err(EnvironmentError::scope_required(scope))
    }
}

/// The middleware (401), then the scope (403), for routes without a payload to decode.
async fn require_scope(state: &RouteState, parts: &Parts, scope: &str) -> Result<(), EnvironmentError> {
    let scopes = state.auth.authenticate(parts).await?;
    check_scope(&scopes, scope)
}

/// `HttpApiDecodeError`: the 400 Effect's HttpApi answers for a request that does not decode.
fn decode_error(message: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "_tag": "HttpApiDecodeError",
            "issues": [],
            "message": message.into(),
        })),
    )
        .into_response()
}

fn ok_json<T: serde::Serialize>(value: &T) -> Response {
    match serde_json::to_value(value) {
        Ok(value) => (StatusCode::OK, Json(value)).into_response(),
        Err(error) => {
            tracing::error!(%error, "could not encode an orchestration response");
            EnvironmentError::internal("internal_error").into_response()
        }
    }
}

fn internal(reason: &str, cause: impl std::fmt::Display) -> Response {
    tracing::error!(reason, %cause, "environment api operation failed");
    EnvironmentError::internal(reason).into_response()
}

async fn snapshot(State(state): State<RouteState>, parts: Parts) -> Response {
    if let Err(error) = require_scope(&state, &parts, ORCHESTRATION_READ_SCOPE).await {
        return error.into_response();
    }
    match state.reads.get_command_read_model().await {
        Ok(model) => ok_json(&model),
        Err(cause) => internal("orchestration_snapshot_failed", cause),
    }
}

async fn shell_snapshot(State(state): State<RouteState>, parts: Parts) -> Response {
    if let Err(error) = require_scope(&state, &parts, ORCHESTRATION_READ_SCOPE).await {
        return error.into_response();
    }
    match state.reads.get_shell_snapshot(false).await {
        Ok(snapshot) => ok_json(&snapshot),
        Err(cause) => internal("orchestration_snapshot_failed", cause),
    }
}

/// `EnvironmentOrchestrationThreadSnapshotQuery`: `reasoningMessages: "true"?`,
/// `turnLimit: FiniteFromString ∩ int ≥ 1`, `beforeCursor: TrimmedNonEmptyString`.
struct ThreadSnapshotQuery {
    reasoning_messages: bool,
    turn_limit: Option<i64>,
    before_cursor: Option<String>,
}

fn parse_thread_snapshot_query(params: &HashMap<String, String>) -> Result<ThreadSnapshotQuery, String> {
    let reasoning_messages = match params.get("reasoningMessages").map(String::as_str) {
        None => false,
        Some("true") => true,
        Some(other) => return Err(format!("reasoningMessages: Expected \"true\", got {other:?}")),
    };
    let turn_limit = match params.get("turnLimit") {
        None => None,
        Some(text) => {
            // `Number(text)`: trimmed, finite; then an integer ≥ 1.
            let number: f64 = crate::js::trim(text)
                .parse()
                .map_err(|_| format!("turnLimit: Expected a finite number, got {text:?}"))?;
            if !number.is_finite() || number.fract() != 0.0 || number < 1.0 {
                return Err(format!("turnLimit: Expected an integer ≥ 1, got {text:?}"));
            }
            Some(number as i64)
        }
    };
    let before_cursor = match params.get("beforeCursor") {
        None => None,
        Some(text) => {
            let trimmed = crate::js::trim(text);
            if trimmed.is_empty() {
                return Err("beforeCursor: Expected a non-empty string".into());
            }
            Some(trimmed.to_string())
        }
    };
    Ok(ThreadSnapshotQuery {
        reasoning_messages,
        turn_limit,
        before_cursor,
    })
}

async fn thread_snapshot(
    State(state): State<RouteState>,
    Path(thread_id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    parts: Parts,
) -> Response {
    // Middleware (401), then the endpoint's request decoding (400), then the scope (403).
    let scopes = match state.auth.authenticate(&parts).await {
        Ok(scopes) => scopes,
        Err(error) => return error.into_response(),
    };
    let query = match parse_thread_snapshot_query(&params) {
        Ok(query) => query,
        Err(message) => return decode_error(message),
    };
    if let Err(error) = check_scope(&scopes, ORCHESTRATION_READ_SCOPE) {
        return error.into_response();
    }
    let window = query.turn_limit.map(|turn_limit| OrchestrationThreadDetailWindow {
        turn_limit: Some(turn_limit),
        before_cursor: query.before_cursor.clone(),
    });
    match state.reads.get_thread_detail_snapshot(&ThreadId::new(thread_id), window).await {
        Ok(Some(snapshot)) => ok_json(&project_thread_detail_snapshot(snapshot, query.reasoning_messages)),
        Ok(None) => EnvironmentError::ResourceNotFound {
            reason: "thread_not_found".into(),
            trace_id: None,
        }
        .into_response(),
        Err(cause) => internal("orchestration_thread_snapshot_failed", cause),
    }
}

async fn dispatch_command(State(state): State<RouteState>, parts: Parts, body: axum::body::Bytes) -> Response {
    let scopes = match state.auth.authenticate(&parts).await {
        Ok(scopes) => scopes,
        Err(error) => return error.into_response(),
    };
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(error) => return decode_error(error.to_string()),
    };
    if let Err(error) = serde_json::from_value::<zc_contracts::ClientOrchestrationCommand>(payload.clone()) {
        return decode_error(error.to_string());
    }
    if let Err(error) = check_scope(&scopes, ORCHESTRATION_OPERATE_SCOPE) {
        return error.into_response();
    }
    match state.dispatch.dispatch(payload).await {
        Ok(result) => ok_json(&result),
        Err(HttpDispatchError::InvalidCommand) => EnvironmentError::RequestInvalid {
            reason: "invalid_command".into(),
            trace_id: None,
        }
        .into_response(),
        Err(HttpDispatchError::Failed(cause)) => internal("orchestration_dispatch_failed", cause),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_thread_snapshot_query_like_the_schema() {
        let params = |pairs: &[(&str, &str)]| -> HashMap<String, String> { pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() };
        let query = parse_thread_snapshot_query(&params(&[("turnLimit", " 3 "), ("reasoningMessages", "true")])).unwrap();
        assert_eq!(query.turn_limit, Some(3));
        assert!(query.reasoning_messages);
        assert!(parse_thread_snapshot_query(&params(&[("turnLimit", "0")])).is_err());
        assert!(parse_thread_snapshot_query(&params(&[("turnLimit", "1.5")])).is_err());
        assert!(parse_thread_snapshot_query(&params(&[("turnLimit", "x")])).is_err());
        assert!(parse_thread_snapshot_query(&params(&[("reasoningMessages", "false")])).is_err());
        assert!(parse_thread_snapshot_query(&params(&[("beforeCursor", "  ")])).is_err());
        assert_eq!(
            parse_thread_snapshot_query(&params(&[("beforeCursor", " abc ")]))
                .unwrap()
                .before_cursor
                .as_deref(),
            Some("abc")
        );
    }
}
