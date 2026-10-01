//! `/mcp`: MCP Streamable HTTP for protocol `2025-06-18`, as Effect's `McpServer.layerHttp`
//! serves it behind `McpHttpServer.ts`'s bearer check.
//!
//! - Every method first needs a registry-issued bearer credential: otherwise `401
//!   {error:"invalid_mcp_credential"}` with `WWW-Authenticate: Bearer`.
//! - `POST` takes one JSON-RPC message (no batches in this protocol version) and answers it
//!   with one JSON body; a notification or a client response gets `202`. `initialize` opens a
//!   session (`mcp-session-id` header); every later request names it and repeats
//!   `mcp-protocol-version`. A request carrying an `Origin` is refused (`403`): agents are not
//!   browsers.
//! - `DELETE` ends a session (the local Effect patch): `400` without `mcp-session-id`, `404`
//!   for an unknown one, `204` otherwise.
//! - `GET`, `PUT`, `PATCH`, `OPTIONS`: `405 Allow: POST` (no server-sent event stream).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use serde_json::{json, Map, Value};

use crate::registry::McpSessionRegistry;
use crate::scope::McpInvocationScope;
use crate::tools::Toolkit;

/// The only protocol revision served (`McpProtocol.v2025_06_18`).
pub const PROTOCOL_VERSION: &str = "2025-06-18";
const SESSION_HEADER: &str = "mcp-session-id";
const VERSION_HEADER: &str = "mcp-protocol-version";

/// `McpServer.layerHttp` options.
#[derive(Debug, Clone)]
pub struct McpHttpOptions {
    /// `serverInfo.name` ("T3 Code" upstream, the brand here).
    pub server_name: String,
    pub server_version: String,
    /// Browser origins allowed to call (none by default).
    pub allowed_origins: Vec<String>,
}

impl Default for McpHttpOptions {
    fn default() -> Self {
        Self {
            server_name: crate::BRAND_NAME.to_owned(),
            server_version: "0.0.0".to_owned(),
            allowed_origins: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
struct Session {
    #[allow(dead_code)]
    client_info: Value,
    log_level: Option<String>,
}

struct McpHttpState {
    registry: McpSessionRegistry,
    toolkit: Toolkit,
    options: McpHttpOptions,
    sessions: Mutex<HashMap<String, Session>>,
}

/// The `/mcp` route.
pub fn router(registry: McpSessionRegistry, toolkit: Toolkit, options: McpHttpOptions) -> Router {
    let state = Arc::new(McpHttpState {
        registry,
        toolkit,
        options,
        sessions: Mutex::new(HashMap::new()),
    });
    Router::new().route("/mcp", any(handle)).with_state(state)
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn empty(status: StatusCode) -> Response {
    (status, Body::empty()).into_response()
}

fn json_response(status: StatusCode, body: &Value, extra: &[(&'static str, String)]) -> Response {
    let mut response = (status, serde_json::to_string(body).unwrap_or_default()).into_response();
    let headers = response.headers_mut();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    for (name, value) in extra {
        if let Ok(value) = HeaderValue::from_str(value) {
            headers.insert(*name, value);
        }
    }
    response
}

fn unauthorized() -> Response {
    let mut response = json_response(
        StatusCode::UNAUTHORIZED,
        &json!({
            "error": "invalid_mcp_credential",
            "message": "A valid provider-scoped MCP bearer credential is required.",
        }),
        &[],
    );
    let headers = response.headers_mut();
    headers.insert("cache-control", HeaderValue::from_static("no-store"));
    headers.insert("www-authenticate", HeaderValue::from_static("Bearer"));
    response
}

/// `mcpMediaTypes`: the media types of an `Accept` / `Content-Type` header, without the ones
/// whose quality is zero or invalid.
fn media_types(value: Option<&str>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    value
        .split(',')
        .filter_map(|part| {
            let mut pieces = part.split(';');
            let media_type = pieces.next().unwrap_or("");
            let quality = pieces
                .map(|parameter| parameter.trim().to_lowercase())
                .find(|parameter| parameter.starts_with("q="));
            if let Some(quality) = quality {
                let valid = quality[2..].trim().parse::<f64>().ok().filter(|q| q.is_finite() && *q > 0.0 && *q <= 1.0);
                valid?;
            }
            Some(media_type.trim().to_lowercase())
        })
        .collect()
}

fn origin_allowed(headers: &HeaderMap, allowed: &[String]) -> bool {
    match header(headers, "origin") {
        None => true,
        Some(origin) => allowed.iter().any(|allowed| allowed == origin),
    }
}

async fn handle(State(state): State<Arc<McpHttpState>>, method: Method, headers: HeaderMap, body: Bytes) -> Response {
    let token = header(&headers, "authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");
    let Some(scope) = state.registry.resolve(token) else {
        // Without this the only symptom of a dead credential is the agent quietly losing the
        // whole `t3-code` toolkit for the rest of its session.
        let reason = if token.is_empty() {
            "missing_bearer_token"
        } else {
            "unknown_or_expired_token"
        };
        tracing::warn!(reason, "rejected MCP request with an unusable credential");
        return unauthorized();
    };
    match method {
        Method::POST => post(&state, &scope, &headers, &body).await,
        Method::DELETE => delete(&state, &headers),
        _ => {
            if !origin_allowed(&headers, &state.options.allowed_origins) {
                return empty(StatusCode::FORBIDDEN);
            }
            let mut response = empty(StatusCode::METHOD_NOT_ALLOWED);
            response.headers_mut().insert("allow", HeaderValue::from_static("POST"));
            response
        }
    }
}

fn delete(state: &McpHttpState, headers: &HeaderMap) -> Response {
    let Some(session_id) = header(headers, SESSION_HEADER) else {
        return empty(StatusCode::BAD_REQUEST);
    };
    if state.sessions.lock().unwrap().remove(session_id).is_none() {
        return empty(StatusCode::NOT_FOUND);
    }
    empty(StatusCode::NO_CONTENT)
}

/// A JSON-RPC error answered before routing (`{code, message, _tag}`).
fn protocol_error(id: Value, code: i64, message: &str, tag: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message, "_tag": tag}})
}

/// A handler failure, as Effect's JSON-RPC serializer writes an `Exit` with a `Fail` cause.
fn cause_error(id: &Value, code: i64, message: &str, tag: Option<&str>) -> Value {
    let mut inner = Map::new();
    inner.insert("code".into(), json!(code));
    inner.insert("message".into(), json!(message));
    if let Some(tag) = tag {
        inner.insert("_tag".into(), json!(tag));
    }
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"_tag": "Cause", "code": code, "message": message, "data": [{"_tag": "Fail", "error": Value::Object(inner)}]},
    })
}

fn result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn invalid_method_parameters(id: &Value) -> Value {
    cause_error(id, -32602, "Invalid method parameters", Some("InvalidParams"))
}

async fn post(state: &McpHttpState, scope: &McpInvocationScope, headers: &HeaderMap, body: &[u8]) -> Response {
    if !origin_allowed(headers, &state.options.allowed_origins) {
        return empty(StatusCode::FORBIDDEN);
    }
    if media_types(header(headers, "content-type")).first().map(String::as_str) != Some("application/json") {
        return empty(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    let accepted = media_types(header(headers, "accept"));
    if !accepted.iter().any(|t| t == "application/json") || !accepted.iter().any(|t| t == "text/event-stream") {
        return empty(StatusCode::NOT_ACCEPTABLE);
    }
    let protocol_version = header(headers, VERSION_HEADER);
    let session_id = header(headers, SESSION_HEADER).map(str::to_owned);
    let session_exists = match &session_id {
        Some(id) => state.sessions.lock().unwrap().contains_key(id),
        None => false,
    };
    if session_id.is_some() && !session_exists {
        return empty(StatusCode::NOT_FOUND);
    }
    let version_rejected =
        protocol_version.is_some_and(|version| version != PROTOCOL_VERSION) || (session_exists && protocol_version != Some(PROTOCOL_VERSION));

    let Ok(input) = serde_json::from_slice::<Value>(body) else {
        if version_rejected {
            return empty(StatusCode::BAD_REQUEST);
        }
        return json_response(StatusCode::OK, &protocol_error(Value::Null, -32700, "Parse error", "ParseError"), &[]);
    };

    let message = match input {
        Value::Array(items) => {
            if version_rejected {
                return empty(StatusCode::BAD_REQUEST);
            }
            if items.is_empty() {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    &protocol_error(Value::Null, -32600, "Invalid Request", "InvalidRequest"),
                    &[],
                );
            }
            // This protocol version takes no batches.
            return empty(StatusCode::BAD_REQUEST);
        }
        Value::Object(message) => message,
        other => {
            if version_rejected {
                return empty(StatusCode::BAD_REQUEST);
            }
            let _ = other;
            return json_response(StatusCode::OK, &protocol_error(Value::Null, -32600, "Invalid Request", "InvalidRequest"), &[]);
        }
    };

    let id = message.get("id").cloned();
    let id_valid = |id: &Option<Value>| id.as_ref().is_none_or(|id| id.is_string() || id.is_number());
    let echo_id = id.clone().filter(|id| id.is_string() || id.is_number()).unwrap_or(Value::Null);
    let is_json_rpc = message.get("jsonrpc") == Some(&json!("2.0"));
    let method = message.get("method").and_then(Value::as_str).map(str::to_owned);
    let is_request = is_json_rpc && id_valid(&id) && method.is_some();
    let response_id_valid = id.as_ref().is_some_and(|id| id.is_string() || id.is_number() || id.is_null());
    let is_response = is_json_rpc && response_id_valid && (message.contains_key("result") != message.contains_key("error"));
    let is_initialize = is_request && method.as_deref() == Some("initialize");

    // Initialize is exempt from the version check: refusing it makes clients fall back to the
    // legacy HTTP+SSE transport.
    if !is_initialize && version_rejected {
        return empty(StatusCode::BAD_REQUEST);
    }
    if !is_request && !is_response {
        return json_response(StatusCode::OK, &protocol_error(echo_id, -32600, "Invalid Request", "InvalidRequest"), &[]);
    }
    if is_initialize && session_id.is_some() {
        return empty(StatusCode::BAD_REQUEST);
    }
    if !is_initialize && is_request && session_id.is_none() {
        return empty(StatusCode::BAD_REQUEST);
    }
    if is_response {
        // An answer to a server-to-client request; none are ever sent over this transport.
        return accepted_response(&[]);
    }

    let method = method.unwrap_or_default();
    let params = message.get("params");
    let session_headers: Vec<(&'static str, String)> = if session_exists {
        vec![(VERSION_HEADER, PROTOCOL_VERSION.to_owned())]
    } else {
        Vec::new()
    };
    let Some(id) = id else {
        // A notification: nothing to answer.
        return accepted_response(&session_headers);
    };

    if method == "initialize" {
        let Some(params) = params.and_then(Value::as_object).filter(|params| {
            params.get("protocolVersion").is_some_and(Value::is_string)
                && params.get("capabilities").is_some_and(Value::is_object)
                && params
                    .get("clientInfo")
                    .is_some_and(|info| info.get("name").is_some_and(Value::is_string) && info.get("version").is_some_and(Value::is_string))
        }) else {
            return json_response(StatusCode::OK, &invalid_method_parameters(&id), &[]);
        };
        let session_id = zc_core::ids::uuid_v4();
        state.sessions.lock().unwrap().insert(
            session_id.clone(),
            Session {
                client_info: params.get("clientInfo").cloned().unwrap_or(Value::Null),
                log_level: None,
            },
        );
        let body = result(
            &id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"logging": {}, "tools": {"listChanged": true}, "completions": {}},
                "serverInfo": {"name": state.options.server_name, "version": state.options.server_version},
            }),
        );
        return json_response(
            StatusCode::OK,
            &body,
            &[(SESSION_HEADER, session_id), (VERSION_HEADER, PROTOCOL_VERSION.to_owned())],
        );
    }

    let session_id = session_id.unwrap_or_default();
    let body = match method.as_str() {
        "ping" => result(&id, json!({})),
        "tools/list" => result(&id, json!({"tools": state.toolkit.list()})),
        "tools/call" => {
            let Some(params) = params
                .and_then(Value::as_object)
                .filter(|params| params.get("name").is_some_and(Value::is_string) && params.get("arguments").is_none_or(Value::is_object))
            else {
                return json_response(StatusCode::OK, &invalid_method_parameters(&id), &session_headers);
            };
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            match state.toolkit.call(name, params.get("arguments"), scope).await {
                Ok(call_result) => result(&id, call_result),
                Err(error) => cause_error(&id, -32602, &error.message(), None),
            }
        }
        "resources/list" => result(&id, json!({"resources": []})),
        "resources/templates/list" => result(&id, json!({"resourceTemplates": []})),
        "prompts/list" => result(&id, json!({"prompts": []})),
        "logging/setLevel" => {
            let level = params.and_then(|params| params.get("level")).and_then(Value::as_str);
            match level {
                Some(level) => {
                    if let Some(session) = state.sessions.lock().unwrap().get_mut(&session_id) {
                        session.log_level = Some(level.to_owned());
                    }
                    result(&id, json!({}))
                }
                None => invalid_method_parameters(&id),
            }
        }
        "completion/complete" => cause_error(&id, -32602, "Unknown completion reference or argument", None),
        _ => cause_error(&id, -32601, &format!("Method not found: {method}"), Some("MethodNotFound")),
    };
    json_response(StatusCode::OK, &body, &session_headers)
}

/// The `202` an answered-by-nothing message gets (the 200-with-empty-body rewrite of
/// `normalizeMcpHttpResponse`).
fn accepted_response(extra: &[(&'static str, String)]) -> Response {
    let mut response = (StatusCode::ACCEPTED, Body::empty()).into_response();
    let headers = response.headers_mut();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    for (name, value) in extra {
        if let Ok(value) = HeaderValue::from_str(value) {
            headers.insert(*name, value);
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_media_types_like_effect() {
        assert_eq!(
            media_types(Some("application/json, text/event-stream")),
            vec!["application/json", "text/event-stream"]
        );
        assert_eq!(media_types(Some("Application/JSON; charset=utf-8")), vec!["application/json"]);
        assert_eq!(media_types(Some("application/json;q=0, text/event-stream")), vec!["text/event-stream"]);
        assert!(media_types(None).is_empty());
    }
}
