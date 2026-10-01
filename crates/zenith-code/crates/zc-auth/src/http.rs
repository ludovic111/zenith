//! The auth group of `EnvironmentHttpApi` (`auth/http.ts`), plus the
//! `EnvironmentAuthenticatedAuth` middleware as an axum extractor for the other groups.
//!
//! | Route | Auth | Answer |
//! |---|---|---|
//! | `GET /api/auth/session` | optional | `AuthSessionState`, always 200 |
//! | `POST /api/auth/browser-session` | — | `AuthBrowserSessionResult` + `Set-Cookie` |
//! | `POST /oauth/token` | — (optional `DPoP`) | `AuthAccessTokenResult` (form body) |
//! | `POST /api/auth/websocket-ticket` | session | `AuthWebSocketTicketResult` |
//! | `POST /api/auth/pairing-token` | `access:write` | `AuthPairingCredentialResult` |
//! | `GET /api/auth/pairing-links` | `access:read` | `AuthPairingLink[]` |
//! | `POST /api/auth/pairing-links/revoke` | `access:write` | `{revoked}` |
//! | `GET /api/auth/clients` | `access:read` | `AuthClientSession[]` |
//! | `POST /api/auth/clients/revoke` | `access:write` | `{revoked}`, 403 on the own session |
//! | `POST /api/auth/clients/revoke-others` | `access:write` | `{revokedCount}` |
//!
//! Like Effect's HttpApi: the session check runs before the body is read; a body that does not
//! decode is an empty 400; a JSON endpoint given another content type answers 415
//! `Unsupported content-type: <type>` (no content type means JSON). Credential responses
//! (browser session, token, ticket) carry `cache-control: no-store` and `pragma: no-cache` on
//! success only. A 401 on a request that used DPoP carries `www-authenticate: DPoP`.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::extract::{ConnectInfo, FromRef, FromRequestParts, Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use http::request::Parts;
use http::{header, HeaderValue, StatusCode};
use serde::Serialize;
use serde_json::{Map, Value};
use zc_contracts::{AuthClientMetadataDeviceType as DeviceType, AuthEnvironmentScope as Scope, JsNumber};
use zc_http::EnvironmentError;

use crate::client_metadata::{derive_auth_client_metadata, PresentedClient};
use crate::cookies::session_set_cookie;
use crate::dpop::verify_request_dpop_proof;
use crate::environment_auth::{header_text, AuthRequest, AuthenticatedSession, CredentialSource, EnvironmentAuth};
use crate::error::ServerAuthError;
use crate::scopes::{parse_scope, STANDARD_CLIENT_SCOPES};
use crate::token::js_trim;

/// Request bodies above this are refused (the API's payloads are tiny).
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// The auth routes, with their state.
pub fn routes(auth: Arc<EnvironmentAuth>) -> Router {
    Router::new()
        .route("/api/auth/session", get(session))
        .route("/api/auth/browser-session", post(browser_session))
        .route("/oauth/token", post(token))
        .route("/api/auth/websocket-ticket", post(websocket_ticket))
        .route("/api/auth/pairing-token", post(pairing_credential))
        .route("/api/auth/pairing-links", get(pairing_links))
        .route("/api/auth/pairing-links/revoke", post(revoke_pairing_link))
        .route("/api/auth/clients", get(clients))
        .route("/api/auth/clients/revoke", post(revoke_client))
        .route("/api/auth/clients/revoke-others", post(revoke_other_clients))
        .with_state(auth)
}

fn path_and_query(parts: &Parts) -> &str {
    parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/")
}

fn peer(parts: &Parts) -> Option<SocketAddr> {
    parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0)
}

/// The [`AuthRequest`] view of a request.
pub fn auth_request(parts: &Parts) -> AuthRequest<'_> {
    AuthRequest {
        method: parts.method.as_str(),
        headers: &parts.headers,
        path_and_query: path_and_query(parts),
        peer: peer(parts),
    }
}

fn json_response(status: StatusCode, value: &impl Serialize) -> Response {
    match serde_json::to_string(value) {
        Ok(body) => (status, [(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))], body).into_response(),
        Err(_) => EnvironmentError::internal("internal_error").into_response(),
    }
}

fn with_credential_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    response
}

fn with_dpop_challenge(mut response: Response) -> Response {
    response.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("DPoP"));
    response
}

fn bad_request() -> Response {
    StatusCode::BAD_REQUEST.into_response()
}

/// `failEnvironmentAuthInvalid` for a credential error.
fn auth_invalid(error: &ServerAuthError) -> EnvironmentError {
    EnvironmentError::AuthInvalid {
        reason: error.credential_reason().to_owned(),
        dpop_failure_reason: error.dpop_failure_reason().map(|r| r.as_str().to_owned()),
        trace_id: None,
    }
}

fn internal(reason: &str, error: &ServerAuthError) -> Response {
    tracing::error!(reason, tag = error.tag(), %error, "environment api operation failed");
    EnvironmentError::internal(reason).into_response()
}

fn request_invalid(reason: &str) -> Response {
    EnvironmentError::RequestInvalid {
        reason: reason.to_owned(),
        trace_id: None,
    }
    .into_response()
}

/// Whether a request authenticates with DPoP (`appendDpopChallengeOnUnauthorized`).
fn uses_dpop(parts: &Parts) -> bool {
    (path_and_query(parts).starts_with("/oauth/token") && parts.headers.contains_key("dpop"))
        || header_text(&parts.headers, "authorization").is_some_and(|a| a.starts_with("DPoP "))
}

/// The `EnvironmentAuthenticatedAuth` middleware: the request's session, or the 401/500
/// response to send instead.
pub async fn authenticate(auth: &EnvironmentAuth, parts: &Parts) -> Result<AuthenticatedSession, Response> {
    match auth.authenticate_request(&auth_request(parts)).await {
        Ok(session) => Ok(session),
        Err(error) if error.is_credential_error() => {
            let response = auth_invalid(&error).into_response();
            Err(if uses_dpop(parts) { with_dpop_challenge(response) } else { response })
        }
        Err(error) => Err(internal("internal_error", &error)),
    }
}

/// `requireEnvironmentScope`: 403 `EnvironmentScopeRequiredError` when the session lacks it.
pub fn require_scope(session: &AuthenticatedSession, scope: Scope) -> Result<(), Response> {
    if session.has_scope(scope) {
        Ok(())
    } else {
        Err(EnvironmentError::scope_required(scope.as_str()).into_response())
    }
}

/// An axum extractor for routes behind `EnvironmentAuthenticatedAuth`: any state from which
/// an `Arc<EnvironmentAuth>` can be taken.
#[derive(Clone, Debug)]
pub struct Authenticated(pub AuthenticatedSession);

impl<S> FromRequestParts<S> for Authenticated
where
    Arc<EnvironmentAuth>: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let auth = Arc::<EnvironmentAuth>::from_ref(state);
        authenticate(&auth, parts).await.map(Authenticated)
    }
}

/// How a JSON or form body failed to decode.
enum PayloadRejection {
    UnsupportedContentType(String),
    Invalid,
}

impl IntoResponse for PayloadRejection {
    fn into_response(self) -> Response {
        match self {
            Self::UnsupportedContentType(content_type) => (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                [(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"))],
                format!("Unsupported content-type: {content_type}"),
            )
                .into_response(),
            Self::Invalid => bad_request(),
        }
    }
}

/// `MediaType.normalize(content-type ?? "application/json")`.
fn content_type(parts: &Parts) -> String {
    let raw = header_text(&parts.headers, "content-type").unwrap_or_else(|| "application/json".into());
    let normalized = raw.to_lowercase();
    let normalized = normalized.trim();
    match normalized.find(';') {
        Some(index) => normalized[..index].trim().to_owned(),
        None => normalized.to_owned(),
    }
}

async fn read_body(body: Body) -> Result<String, PayloadRejection> {
    let bytes = to_bytes(body, MAX_BODY_BYTES).await.map_err(|_| PayloadRejection::Invalid)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// A JSON body: `None` for an empty body (decoded as `undefined`, which no struct accepts).
async fn json_body(parts: &Parts, body: Body) -> Result<Map<String, Value>, PayloadRejection> {
    let content_type = content_type(parts);
    if content_type != "application/json" {
        return Err(PayloadRejection::UnsupportedContentType(content_type));
    }
    let text = read_body(body).await?;
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(PayloadRejection::Invalid),
    }
}

/// `TrimmedNonEmptyString` field; `Err` when present but invalid, `Ok(None)` when absent.
fn optional_trimmed(map: &Map<String, Value>, key: &str) -> Result<Option<String>, PayloadRejection> {
    match map.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => {
            let trimmed = js_trim(s);
            if trimmed.is_empty() {
                Err(PayloadRejection::Invalid)
            } else {
                Ok(Some(trimmed.to_owned()))
            }
        }
        Some(_) => Err(PayloadRejection::Invalid),
    }
}

fn required_trimmed(map: &Map<String, Value>, key: &str) -> Result<String, PayloadRejection> {
    optional_trimmed(map, key)?.ok_or(PayloadRejection::Invalid)
}

fn optional_scopes(map: &Map<String, Value>, key: &str) -> Result<Option<Vec<Scope>>, PayloadRejection> {
    match map.get(key) {
        None => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().and_then(parse_scope))
            .collect::<Option<Vec<_>>>()
            .map(Some)
            .ok_or(PayloadRejection::Invalid),
        Some(_) => Err(PayloadRejection::Invalid),
    }
}

/// `GET /api/auth/session`.
async fn session(State(auth): State<Arc<EnvironmentAuth>>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let state = match auth.get_session_state(&auth_request(&parts)).await {
        Ok(state) => state,
        Err(error) => return internal("internal_error", &error),
    };
    let mut response = json_response(StatusCode::OK, &state);
    // A browser still holding the legacy `t3_session` cookie gets it moved to the scoped name.
    if let (true, Some(expires_at), Some(credential)) = (
        state.authenticated && state.session_method == Some(zc_contracts::ServerAuthSessionMethod::BrowserSessionCookie),
        state.expires_at,
        auth.select_credential(&parts.headers),
    ) {
        if credential.source == CredentialSource::LegacyCookie {
            let cookie = session_set_cookie(auth.sessions().cookie_name(), &credential.token, expires_at.as_millis());
            if let Ok(value) = HeaderValue::from_str(&cookie) {
                response.headers_mut().append(header::SET_COOKIE, value);
            }
            response = with_credential_headers(response);
        }
    }
    response
}

/// `POST /api/auth/browser-session`.
async fn browser_session(State(auth): State<Arc<EnvironmentAuth>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let credential = match json_body(&parts, body).await.and_then(|map| required_trimmed(&map, "credential")) {
        Ok(credential) => credential,
        Err(rejection) => return rejection.into_response(),
    };
    let request_view = auth_request(&parts);
    let metadata = derive_auth_client_metadata(request_view.header("user-agent").as_deref(), request_view.remote_address().as_deref(), None);
    match auth.create_browser_session(&credential, metadata).await {
        Ok(exchange) => {
            let cookie = session_set_cookie(auth.sessions().cookie_name(), &exchange.session_token, exchange.response.expires_at.as_millis());
            let Ok(cookie) = HeaderValue::from_str(&cookie) else {
                return EnvironmentError::internal("browser_session_cookie_failed").into_response();
            };
            let mut response = json_response(StatusCode::OK, &exchange.response);
            response.headers_mut().append(header::SET_COOKIE, cookie);
            with_credential_headers(response)
        }
        Err(error) if error.is_credential_error() => auth_invalid(&error).into_response(),
        Err(error) => internal("browser_session_issuance_failed", &error),
    }
}

/// `parseAllowedOAuthScope`: space-separated RFC 6749 scope tokens, all known; deduplicated in
/// first-seen order. `None` when invalid.
pub fn parse_allowed_oauth_scope(value: &str) -> Option<Vec<Scope>> {
    if value.is_empty() {
        return None;
    }
    let mut scopes = Vec::new();
    for token in value.split(' ') {
        let valid = !token.is_empty()
            && token
                .chars()
                .all(|c| c == '\u{21}' || ('\u{23}'..='\u{5b}').contains(&c) || ('\u{5d}'..='\u{7e}').contains(&c));
        if !valid {
            return None;
        }
        let scope = parse_scope(token)?;
        if !scopes.contains(&scope) {
            scopes.push(scope);
        }
    }
    Some(scopes)
}

struct TokenRequest {
    subject_token: String,
    scope: Option<String>,
    client_label: Option<String>,
    client_device_type: Option<DeviceType>,
    client_os: Option<String>,
}

/// `AuthTokenExchangeRequest` from a form body. A repeated key decodes as an array in Effect,
/// which no field accepts.
fn decode_token_request(text: &str) -> Option<TokenRequest> {
    let mut fields: Vec<(String, String)> = Vec::new();
    for (key, value) in url::form_urlencoded::parse(text.as_bytes()) {
        if fields.iter().any(|(k, _)| *k == key) {
            return None;
        }
        fields.push((key.into_owned(), value.into_owned()));
    }
    let get = |key: &str| fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
    let trimmed = |key: &str| -> Result<Option<String>, ()> {
        match get(key) {
            None => Ok(None),
            Some(v) => {
                let t = js_trim(v);
                if t.is_empty() {
                    Err(())
                } else {
                    Ok(Some(t.to_owned()))
                }
            }
        }
    };
    if get("grant_type")? != "urn:ietf:params:oauth:grant-type:token-exchange"
        || get("subject_token_type")? != "urn:t3:params:oauth:token-type:environment-bootstrap"
        || get("requested_token_type")? != "urn:ietf:params:oauth:token-type:access_token"
    {
        return None;
    }
    let client_device_type = match get("client_device_type") {
        None => None,
        Some(v) => Some(DeviceType::ALL.iter().copied().find(|d| d.as_str() == v)?),
    };
    Some(TokenRequest {
        subject_token: trimmed("subject_token").ok()??,
        scope: trimmed("scope").ok()?,
        client_label: trimmed("client_label").ok()?,
        client_device_type,
        client_os: trimmed("client_os").ok()?,
    })
}

/// `POST /oauth/token`: RFC 8693 token exchange of a pairing credential for an access token.
async fn token(State(auth): State<Arc<EnvironmentAuth>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let content_type = content_type(&parts);
    if content_type != "application/x-www-form-urlencoded" {
        return PayloadRejection::UnsupportedContentType(content_type).into_response();
    }
    let text = match read_body(body).await {
        Ok(text) => text,
        Err(rejection) => return rejection.into_response(),
    };
    let Some(payload) = decode_token_request(&text) else {
        return bad_request();
    };
    let requested_scopes = match payload.scope.as_deref() {
        None => None,
        Some(scope) => match parse_allowed_oauth_scope(scope) {
            Some(scopes) => Some(scopes),
            None => return request_invalid("invalid_scope"),
        },
    };
    let request_view = auth_request(&parts);
    let proof = request_view.header("dpop").filter(|p| !p.is_empty());
    let proof_key_thumbprint = match proof {
        None => None,
        Some(proof) => match verify_request_dpop_proof(
            auth.secrets(),
            Some(&proof),
            request_view.method,
            request_view.url().as_deref(),
            auth.now(),
            None,
            None,
        )
        .await
        {
            Ok(thumbprint) => Some(thumbprint),
            Err(error) if error.is_credential_error() => {
                return with_dpop_challenge(
                    EnvironmentError::AuthInvalid {
                        reason: "invalid_credential".into(),
                        dpop_failure_reason: error.dpop_failure_reason().map(|r| r.as_str().to_owned()),
                        trace_id: None,
                    }
                    .into_response(),
                )
            }
            Err(error) => return internal("access_token_issuance_failed", &error),
        },
    };
    let metadata = derive_auth_client_metadata(
        request_view.header("user-agent").as_deref(),
        request_view.remote_address().as_deref(),
        Some(&PresentedClient {
            label: payload.client_label,
            device_type: payload.client_device_type,
            os: payload.client_os,
        }),
    );
    match auth
        .exchange_bootstrap_credential_for_access_token(&payload.subject_token, requested_scopes, metadata, proof_key_thumbprint)
        .await
    {
        Ok(result) => with_credential_headers(json_response(StatusCode::OK, &result)),
        Err(error) if error.is_credential_error() => auth_invalid(&error).into_response(),
        Err(ServerAuthError::InvalidScope) => request_invalid("invalid_scope"),
        Err(ServerAuthError::ScopeNotGranted) => request_invalid("scope_not_granted"),
        Err(error) => internal("access_token_issuance_failed", &error),
    }
}

/// `POST /api/auth/websocket-ticket`.
async fn websocket_ticket(State(auth): State<Arc<EnvironmentAuth>>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let session = match authenticate(&auth, &parts).await {
        Ok(session) => session,
        Err(response) => return response,
    };
    with_credential_headers(json_response(StatusCode::OK, &auth.issue_websocket_ticket(&session.session_id)))
}

/// `POST /api/auth/pairing-token`.
async fn pairing_credential(State(auth): State<Arc<EnvironmentAuth>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let session = match authenticate(&auth, &parts).await {
        Ok(session) => session,
        Err(response) => return response,
    };
    let (label, scopes) = match json_body(&parts, body)
        .await
        .and_then(|map| Ok((optional_trimmed(&map, "label")?, optional_scopes(&map, "scopes")?)))
    {
        Ok(payload) => payload,
        Err(rejection) => return rejection.into_response(),
    };
    if let Err(response) = require_scope(&session, Scope::AccessWrite) {
        return response;
    }
    let delegated = scopes.clone().unwrap_or_else(|| STANDARD_CLIENT_SCOPES.to_vec());
    let mut unique = delegated.clone();
    unique.sort();
    unique.dedup();
    if delegated.is_empty() || unique.len() != delegated.len() {
        return request_invalid("invalid_scope");
    }
    for scope in &delegated {
        if let Err(response) = require_scope(&session, *scope) {
            return response;
        }
    }
    match auth.issue_pairing_credential(label, scopes).await {
        Ok(result) => json_response(StatusCode::OK, &result),
        Err(error) => internal("pairing_credential_issuance_failed", &error),
    }
}

/// `GET /api/auth/pairing-links`.
async fn pairing_links(State(auth): State<Arc<EnvironmentAuth>>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let session = match authenticate(&auth, &parts).await {
        Ok(session) => session,
        Err(response) => return response,
    };
    if let Err(response) = require_scope(&session, Scope::AccessRead) {
        return response;
    }
    match auth.list_pairing_links(None).await {
        Ok(links) => json_response(StatusCode::OK, &links),
        Err(error) => internal("pairing_links_load_failed", &error),
    }
}

/// `POST /api/auth/pairing-links/revoke`.
async fn revoke_pairing_link(State(auth): State<Arc<EnvironmentAuth>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let session = match authenticate(&auth, &parts).await {
        Ok(session) => session,
        Err(response) => return response,
    };
    let id = match json_body(&parts, body).await.and_then(|map| required_trimmed(&map, "id")) {
        Ok(id) => id,
        Err(rejection) => return rejection.into_response(),
    };
    if let Err(response) = require_scope(&session, Scope::AccessWrite) {
        return response;
    }
    match auth.revoke_pairing_link(&id).await {
        Ok(revoked) => json_response(StatusCode::OK, &serde_json::json!({ "revoked": revoked })),
        Err(error) => internal("pairing_link_revoke_failed", &error),
    }
}

/// `GET /api/auth/clients`.
async fn clients(State(auth): State<Arc<EnvironmentAuth>>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let session = match authenticate(&auth, &parts).await {
        Ok(session) => session,
        Err(response) => return response,
    };
    if let Err(response) = require_scope(&session, Scope::AccessRead) {
        return response;
    }
    match auth.list_client_sessions(&session.session_id).await {
        Ok(sessions) => json_response(StatusCode::OK, &sessions),
        Err(error) => internal("client_sessions_load_failed", &error),
    }
}

/// `POST /api/auth/clients/revoke`.
async fn revoke_client(State(auth): State<Arc<EnvironmentAuth>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let session = match authenticate(&auth, &parts).await {
        Ok(session) => session,
        Err(response) => return response,
    };
    let target = match json_body(&parts, body).await.and_then(|map| required_trimmed(&map, "sessionId")) {
        Ok(target) => target,
        Err(rejection) => return rejection.into_response(),
    };
    if let Err(response) = require_scope(&session, Scope::AccessWrite) {
        return response;
    }
    match auth.revoke_client_session(&session.session_id, &target).await {
        Ok(revoked) => json_response(StatusCode::OK, &serde_json::json!({ "revoked": revoked })),
        Err(ServerAuthError::ForbiddenOperation) => EnvironmentError::OperationForbidden {
            reason: "current_session_revoke_not_allowed".into(),
            trace_id: None,
        }
        .into_response(),
        Err(error) => internal("client_session_revoke_failed", &error),
    }
}

/// `POST /api/auth/clients/revoke-others`.
async fn revoke_other_clients(State(auth): State<Arc<EnvironmentAuth>>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let session = match authenticate(&auth, &parts).await {
        Ok(session) => session,
        Err(response) => return response,
    };
    if let Err(response) = require_scope(&session, Scope::AccessWrite) {
        return response;
    }
    match auth.revoke_other_client_sessions(&session.session_id).await {
        Ok(count) => json_response(StatusCode::OK, &serde_json::json!({ "revokedCount": JsNumber(count as f64) })),
        Err(error) => internal("client_session_revoke_failed", &error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_oauth_scopes_like_rfc_6749() {
        assert_eq!(
            parse_allowed_oauth_scope("access:read access:read orchestration:read"),
            Some(vec![Scope::AccessRead, Scope::OrchestrationRead])
        );
        assert_eq!(parse_allowed_oauth_scope(""), None);
        assert_eq!(parse_allowed_oauth_scope("access:read  orchestration:read"), None);
        assert_eq!(parse_allowed_oauth_scope("unknown:scope"), None);
        assert_eq!(parse_allowed_oauth_scope("access:read\"x"), None);
    }

    #[test]
    fn decodes_token_exchange_forms() {
        let base = "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Atoken-exchange&subject_token=+ABC+&subject_token_type=urn%3At3%3Aparams%3Aoauth%3Atoken-type%3Aenvironment-bootstrap&requested_token_type=urn%3Aietf%3Aparams%3Aoauth%3Atoken-type%3Aaccess_token";
        let ok = decode_token_request(base).unwrap();
        assert_eq!(ok.subject_token, "ABC");
        assert_eq!(ok.scope, None);
        assert!(decode_token_request(&format!("{base}&scope=")).is_none());
        assert!(decode_token_request(&format!("{base}&subject_token=again")).is_none());
        assert!(decode_token_request(&format!("{base}&client_device_type=toaster")).is_none());
        let full = decode_token_request(&format!("{base}&client_device_type=mobile&client_label=Phone")).unwrap();
        assert_eq!(full.client_device_type, Some(DeviceType::Mobile));
        assert_eq!(full.client_label.as_deref(), Some("Phone"));
        assert!(decode_token_request("subject_token=x").is_none());
    }
}
