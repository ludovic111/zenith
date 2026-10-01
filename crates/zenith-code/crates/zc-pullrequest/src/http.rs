//! `pullRequest/http.ts`: `POST /api/pull-requests/diff`, the `pullRequests` group of the
//! environment HTTP API. The patch is often the largest PR payload, so it is served over HTTP
//! (compression, flow control) rather than the WebSocket.
//!
//! Order of checks, like Effect's HttpApi: authentication (401), payload decode (400
//! `HttpApiDecodeError`), scope `orchestration:read` (403), then the service: a
//! `PullRequestUnavailableError` answers 503 and a `PullRequestOperationError` 502, with the
//! error's own encoding as the body.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use zc_contracts::PullRequestDiffInput;
use zc_http::EnvironmentError;
use zc_projections::http::EnvironmentAuth;

use crate::decode::{decode, PayloadSchema};
use crate::error::PullRequestError;

/// The route's path.
pub const DIFF_PATH: &str = "/api/pull-requests/diff";
const ORCHESTRATION_READ_SCOPE: &str = "orchestration:read";

/// What the route reads the diff from (the pull request service).
#[async_trait::async_trait]
pub trait DiffSource: Send + Sync {
    async fn diff(&self, input: PullRequestDiffInput) -> Result<zc_contracts::PullRequestDiffResult, PullRequestError>;
}

#[derive(Clone)]
struct RouteState {
    source: Arc<dyn DiffSource>,
    auth: Arc<dyn EnvironmentAuth>,
}

/// The route, ready to merge into the server's router.
pub fn router(source: Arc<dyn DiffSource>, auth: Arc<dyn EnvironmentAuth>) -> Router {
    Router::new().route(DIFF_PATH, post(diff)).with_state(RouteState { source, auth })
}

/// `HttpApiDecodeError` (400).
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

/// The status a service failure answers with (`httpApiStatus` of the contract errors).
pub fn error_response(error: &PullRequestError) -> Response {
    let status = match error {
        PullRequestError::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
        PullRequestError::Operation { .. } => StatusCode::BAD_GATEWAY,
    };
    (status, Json(error.to_wire())).into_response()
}

async fn diff(State(state): State<RouteState>, parts: Parts, body: Bytes) -> Response {
    let scopes = match state.auth.authenticate(&parts).await {
        Ok(scopes) => scopes,
        Err(error) => return error.into_response(),
    };
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(error) => return decode_error(error.to_string()),
    };
    let input: PullRequestDiffInput = match decode(PayloadSchema::Diff, payload) {
        Ok(input) => input,
        Err(issue) => return decode_error(issue.0),
    };
    if !scopes.iter().any(|scope| scope == ORCHESTRATION_READ_SCOPE) {
        return EnvironmentError::scope_required(ORCHESTRATION_READ_SCOPE).into_response();
    }
    match state.source.diff(input).await {
        Ok(result) => match serde_json::to_value(&result) {
            Ok(value) => (StatusCode::OK, Json(value)).into_response(),
            Err(error) => {
                tracing::error!(%error, "could not encode a pull request diff");
                EnvironmentError::internal("internal_error").into_response()
            }
        },
        Err(error) => error_response(&error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    use zc_contracts::PullRequestDiffResult;

    struct Scopes(Vec<String>);

    #[async_trait::async_trait]
    impl EnvironmentAuth for Scopes {
        async fn authenticate(&self, parts: &Parts) -> Result<Vec<String>, EnvironmentError> {
            if parts.headers.contains_key("authorization") {
                Ok(self.0.clone())
            } else {
                Err(EnvironmentError::missing_credential())
            }
        }
    }

    struct Fixed(Result<PullRequestDiffResult, PullRequestError>);

    #[async_trait::async_trait]
    impl DiffSource for Fixed {
        async fn diff(&self, input: PullRequestDiffInput) -> Result<PullRequestDiffResult, PullRequestError> {
            assert_eq!(input.repository, "acme/widgets");
            self.0.clone()
        }
    }

    async fn call(source: Fixed, scopes: &[&str], authorized: bool, body: Value) -> (StatusCode, Value) {
        let app = router(Arc::new(source), Arc::new(Scopes(scopes.iter().map(|s| s.to_string()).collect())));
        let mut request = Request::post(DIFF_PATH).header("content-type", "application/json");
        if authorized {
            request = request.header("authorization", "Bearer test");
        }
        let response = app.oneshot(request.body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    fn ok() -> Fixed {
        Fixed(Ok(PullRequestDiffResult {
            patch: "diff --git a/x b/x\n".into(),
            truncated: false,
            next_cursor: None,
            omitted_file_stats: None,
        }))
    }

    fn payload() -> Value {
        json!({"projectId": "project-1", "repository": " acme/widgets ", "number": 3})
    }

    #[tokio::test]
    async fn serves_the_diff_to_a_reader() {
        let (status, body) = call(ok(), &["orchestration:read"], true, payload()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"patch": "diff --git a/x b/x\n", "truncated": false, "nextCursor": null}));
    }

    #[tokio::test]
    async fn authenticates_then_decodes_then_checks_the_scope() {
        assert_eq!(call(ok(), &["orchestration:read"], false, payload()).await.0, StatusCode::UNAUTHORIZED);
        let (status, body) = call(ok(), &[], true, json!({"projectId": "p"})).await;
        assert_eq!((status, body["_tag"].clone()), (StatusCode::BAD_REQUEST, json!("HttpApiDecodeError")));
        assert_eq!(call(ok(), &[], true, payload()).await.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn maps_service_failures_to_their_statuses() {
        let unavailable = Fixed(Err(PullRequestError::unavailable(
            zc_contracts::PullRequestUnavailableReason::ProviderUnsupported,
        )));
        let (status, body) = call(unavailable, &["orchestration:read"], true, payload()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, json!({"_tag": "PullRequestUnavailableError", "reason": "provider-unsupported"}));
        let failed = Fixed(Err(PullRequestError::operation("diff", "The diff could not be read.")));
        let (status, body) = call(failed, &["orchestration:read"], true, payload()).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(
            body,
            json!({"_tag": "PullRequestOperationError", "operation": "diff", "detail": "The diff could not be read."})
        );
    }
}
