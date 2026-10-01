//! Typed JSON error responses (`packages/contracts/src/environmentHttp.ts`, plan §1.2):
//! a tagged error as body, with the status of its `httpApiStatus` annotation.
//!
//! ```http
//! HTTP/1.1 401
//! content-type: application/json
//!
//! {"_tag":"EnvironmentAuthInvalidError","code":"auth_invalid","reason":"missing_credential","traceId":"…"}
//! ```
//!
//! Keys are written in the schema's declaration order. `traceId` is the request's trace
//! id (32 hex characters, like an Effect span's); one is drawn if the caller has none.
//! When zc-contracts lands, its generated error types can replace the reason strings
//! here: the wire shape stays the same.

use axum::response::{IntoResponse, Response};
use http::{header, HeaderValue, StatusCode};
use rand::Rng;
use serde_json::{Map, Value};

/// A random trace id: 16 bytes as 32 lowercase hex characters.
pub fn new_trace_id() -> String {
    let bytes: [u8; 16] = rand::rng().random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The common environment API errors (`EnvironmentHttpCommonError`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvironmentError {
    /// 400. `reason`: `invalid_scope | scope_not_granted | invalid_command`.
    RequestInvalid { reason: String, trace_id: Option<String> },
    /// 401. `reason`: `missing_credential | invalid_credential`.
    AuthInvalid {
        reason: String,
        dpop_failure_reason: Option<String>,
        trace_id: Option<String>,
    },
    /// 403, `code: "insufficient_scope"`.
    ScopeRequired { required_scope: String, trace_id: Option<String> },
    /// 403. `reason`: `current_session_revoke_not_allowed`.
    OperationForbidden { reason: String, trace_id: Option<String> },
    /// 404. `reason`: `thread_not_found`.
    ResourceNotFound { reason: String, trace_id: Option<String> },
    /// 500. `reason` from `EnvironmentInternalErrorReason` (`internal_error`, …).
    Internal { reason: String, trace_id: Option<String> },
}

impl EnvironmentError {
    pub fn auth_invalid(reason: impl Into<String>) -> Self {
        Self::AuthInvalid {
            reason: reason.into(),
            dpop_failure_reason: None,
            trace_id: None,
        }
    }

    pub fn missing_credential() -> Self {
        Self::auth_invalid("missing_credential")
    }

    pub fn invalid_credential() -> Self {
        Self::auth_invalid("invalid_credential")
    }

    pub fn scope_required(scope: impl Into<String>) -> Self {
        Self::ScopeRequired {
            required_scope: scope.into(),
            trace_id: None,
        }
    }

    pub fn internal(reason: impl Into<String>) -> Self {
        Self::Internal {
            reason: reason.into(),
            trace_id: None,
        }
    }

    /// Sets the trace id written in the body.
    pub fn with_trace_id(mut self, id: impl Into<String>) -> Self {
        let id = Some(id.into());
        match &mut self {
            Self::RequestInvalid { trace_id, .. }
            | Self::AuthInvalid { trace_id, .. }
            | Self::ScopeRequired { trace_id, .. }
            | Self::OperationForbidden { trace_id, .. }
            | Self::ResourceNotFound { trace_id, .. }
            | Self::Internal { trace_id, .. } => *trace_id = id,
        }
        self
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::RequestInvalid { .. } => StatusCode::BAD_REQUEST,
            Self::AuthInvalid { .. } => StatusCode::UNAUTHORIZED,
            Self::ScopeRequired { .. } | Self::OperationForbidden { .. } => StatusCode::FORBIDDEN,
            Self::ResourceNotFound { .. } => StatusCode::NOT_FOUND,
            Self::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The encoded error, as the client decodes it.
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        let trace = |t: &Option<String>| Value::String(t.clone().unwrap_or_else(new_trace_id));
        match self {
            Self::RequestInvalid { reason, trace_id } => {
                map.insert("_tag".into(), "EnvironmentRequestInvalidError".into());
                map.insert("code".into(), "invalid_request".into());
                map.insert("reason".into(), reason.as_str().into());
                map.insert("traceId".into(), trace(trace_id));
            }
            Self::AuthInvalid {
                reason,
                dpop_failure_reason,
                trace_id,
            } => {
                map.insert("_tag".into(), "EnvironmentAuthInvalidError".into());
                map.insert("code".into(), "auth_invalid".into());
                map.insert("reason".into(), reason.as_str().into());
                if let Some(dpop) = dpop_failure_reason {
                    map.insert("dpopFailureReason".into(), dpop.as_str().into());
                }
                map.insert("traceId".into(), trace(trace_id));
            }
            Self::ScopeRequired { required_scope, trace_id } => {
                map.insert("_tag".into(), "EnvironmentScopeRequiredError".into());
                map.insert("code".into(), "insufficient_scope".into());
                map.insert("requiredScope".into(), required_scope.as_str().into());
                map.insert("traceId".into(), trace(trace_id));
            }
            Self::OperationForbidden { reason, trace_id } => {
                map.insert("_tag".into(), "EnvironmentOperationForbiddenError".into());
                map.insert("code".into(), "operation_forbidden".into());
                map.insert("reason".into(), reason.as_str().into());
                map.insert("traceId".into(), trace(trace_id));
            }
            Self::ResourceNotFound { reason, trace_id } => {
                map.insert("_tag".into(), "EnvironmentResourceNotFoundError".into());
                map.insert("code".into(), "not_found".into());
                map.insert("reason".into(), reason.as_str().into());
                map.insert("traceId".into(), trace(trace_id));
            }
            Self::Internal { reason, trace_id } => {
                map.insert("_tag".into(), "EnvironmentInternalError".into());
                map.insert("code".into(), "internal_error".into());
                map.insert("reason".into(), reason.as_str().into());
                map.insert("traceId".into(), trace(trace_id));
            }
        }
        Value::Object(map)
    }
}

impl std::fmt::Display for EnvironmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.status(), self.to_json())
    }
}

impl std::error::Error for EnvironmentError {}

impl IntoResponse for EnvironmentError {
    fn into_response(self) -> Response {
        TaggedError::new(self.status(), self.to_json()).into_response()
    }
}

/// Any tagged error with a declared status: the cloud group's
/// `EnvironmentHttp{BadRequest,Unauthorized,Forbidden,InternalServer,Conflict}Error`
/// (`{"_tag","message"}`), `EnvironmentCloudEndpointUnavailableError` (503), or a
/// generated contracts error.
#[derive(Clone, Debug, PartialEq)]
pub struct TaggedError {
    pub status: StatusCode,
    pub body: Value,
}

impl TaggedError {
    pub fn new(status: StatusCode, body: Value) -> Self {
        Self { status, body }
    }

    /// `{"_tag": tag, "message": message}`.
    pub fn with_message(status: StatusCode, tag: &str, message: impl Into<String>) -> Self {
        let mut map = Map::new();
        map.insert("_tag".into(), tag.into());
        map.insert("message".into(), Value::String(message.into()));
        Self::new(status, Value::Object(map))
    }

    pub fn from_serialize<E: serde::Serialize>(status: StatusCode, error: &E) -> Self {
        let body = serde_json::to_value(error).unwrap_or_else(|e| {
            serde_json::json!({"_tag": "EnvironmentInternalError", "code": "internal_error",
                "reason": "internal_error", "traceId": new_trace_id(), "detail": e.to_string()})
        });
        Self::new(status, body)
    }
}

impl IntoResponse for TaggedError {
    fn into_response(self) -> Response {
        (
            self.status,
            [(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))],
            self.body.to_string(),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_and_statuses() {
        let e = EnvironmentError::missing_credential().with_trace_id("abc");
        assert_eq!(e.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            e.to_json().to_string(),
            r#"{"_tag":"EnvironmentAuthInvalidError","code":"auth_invalid","reason":"missing_credential","traceId":"abc"}"#
        );
        let e = EnvironmentError::AuthInvalid {
            reason: "invalid_credential".into(),
            dpop_failure_reason: Some("invalid_proof".into()),
            trace_id: Some("t".into()),
        };
        assert_eq!(
            e.to_json().to_string(),
            r#"{"_tag":"EnvironmentAuthInvalidError","code":"auth_invalid","reason":"invalid_credential","dpopFailureReason":"invalid_proof","traceId":"t"}"#
        );
        let e = EnvironmentError::scope_required("access:write").with_trace_id("t");
        assert_eq!(e.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            e.to_json().to_string(),
            r#"{"_tag":"EnvironmentScopeRequiredError","code":"insufficient_scope","requiredScope":"access:write","traceId":"t"}"#
        );
        assert_eq!(EnvironmentError::internal("internal_error").status(), StatusCode::INTERNAL_SERVER_ERROR);
        let generated = EnvironmentError::internal("internal_error").to_json();
        let id = generated["traceId"].as_str().unwrap();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn message_errors() {
        let e = TaggedError::with_message(StatusCode::CONFLICT, "EnvironmentHttpConflictError", "taken");
        assert_eq!(e.body.to_string(), r#"{"_tag":"EnvironmentHttpConflictError","message":"taken"}"#);
    }
}
