//! `GET /api/zenith/sessions`: the sessions and their totals, for the web app's Sessions page.
//!
//! ```http
//! GET /api/zenith/sessions?limit=100&tz=Europe/Paris
//! Cookie: t3_session_…=…            (or Authorization: Bearer …)
//!
//! 200 {"generatedAt":…,"timeZone":"Europe/Paris","sessions":[…],"totals":{…}}
//! ```
//!
//! - `limit`: how many sessions to send, most recent first (default 100, at most 2000).
//!   Totals always cover every session.
//! - `tz`: an IANA zone for the per-day counts ("today", `perDay`); the server's own zone
//!   when absent or unknown.
//!
//! Authenticated like the other API routes, through [`HttpAuthenticator`], and needs the
//! `orchestration:read` scope: 401 `EnvironmentAuthInvalidError` without a valid credential,
//! 403 `EnvironmentScopeRequiredError` without the scope. Answers carry
//! `Cache-Control: no-store`.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{ConnectInfo, Query, Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use http::{header, HeaderMap, HeaderValue, Uri};
use jiff::tz::TimeZone;
use serde::Deserialize;
use zc_http::{EnvironmentError, UpgradeRequest, WsAuthenticator, WsQuery};
use zc_rpc::AuthContext;

use crate::model::ProjectRoot;
use crate::overview::overview;
use crate::reader::SessionReader;

pub const SESSIONS_PATH: &str = "/api/zenith/sessions";

/// The scope the route needs: sessions are read-only, like threads.
pub const REQUIRED_SCOPE: &str = "orchestration:read";

pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 2000;

/// What an HTTP authenticator sees of a request.
#[derive(Debug)]
pub struct HttpAuthRequest<'a> {
    pub headers: &'a HeaderMap,
    pub uri: &'a Uri,
    pub peer: Option<SocketAddr>,
}

/// The auth hook of the typed HTTP routes. zc-auth implements it with the server's
/// credential selection (session cookie, then `Authorization: Bearer`, then DPoP, plan
/// §2.4); until then [`WsAuthBridge`] reuses the `/ws` authenticator.
#[async_trait]
pub trait HttpAuthenticator: Send + Sync + 'static {
    /// The session behind the request, or the error to answer (401 / 500).
    async fn authenticate(&self, request: &HttpAuthRequest<'_>) -> Result<AuthContext, EnvironmentError>;
}

/// Authenticates HTTP requests with a [`WsAuthenticator`], as an upgrade without a ticket
/// would be (cookie or bearer). For the dev server, and for zc-auth if its HTTP and `/ws`
/// checks stay one.
#[derive(Clone)]
pub struct WsAuthBridge(pub Arc<dyn WsAuthenticator>);

#[async_trait]
impl HttpAuthenticator for WsAuthBridge {
    async fn authenticate(&self, request: &HttpAuthRequest<'_>) -> Result<AuthContext, EnvironmentError> {
        let query = WsQuery::default();
        let upgrade = UpgradeRequest {
            headers: request.headers,
            uri: request.uri,
            query: &query,
            peer: request.peer,
        };
        self.0.authenticate(&upgrade).await
    }
}

/// Where the project folders come from: the orchestration read model's projects (and their
/// threads' worktrees) once the server has one.
#[async_trait]
pub trait ProjectRoots: Send + Sync + 'static {
    async fn project_roots(&self) -> Vec<ProjectRoot>;
}

/// A fixed list of project folders.
#[derive(Clone, Debug, Default)]
pub struct StaticProjectRoots(pub Vec<ProjectRoot>);

#[async_trait]
impl ProjectRoots for StaticProjectRoots {
    async fn project_roots(&self) -> Vec<ProjectRoot> {
        self.0.clone()
    }
}

/// What the route needs.
#[derive(Clone)]
pub struct SessionsApi {
    pub reader: Arc<SessionReader>,
    pub projects: Arc<dyn ProjectRoots>,
    pub auth: Arc<dyn HttpAuthenticator>,
}

/// `GET /api/zenith/sessions`, to merge into the server's route table (inside its CORS,
/// compression and readiness layers).
pub fn router(api: SessionsApi) -> Router {
    Router::new().route(SESSIONS_PATH, get(sessions)).with_state(api)
}

#[derive(Debug, Default, Deserialize)]
struct SessionsQuery {
    limit: Option<String>,
    tz: Option<String>,
}

async fn sessions(State(api): State<SessionsApi>, request: Request) -> Result<Response, EnvironmentError> {
    let peer = request.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0);
    let auth = api
        .auth
        .authenticate(&HttpAuthRequest {
            headers: request.headers(),
            uri: request.uri(),
            peer,
        })
        .await?;
    if !auth.has_scope(REQUIRED_SCOPE) {
        return Err(EnvironmentError::scope_required(REQUIRED_SCOPE));
    }
    let query = Query::<SessionsQuery>::try_from_uri(request.uri()).map(|q| q.0).unwrap_or_default();
    let limit = query.limit.and_then(|l| l.trim().parse::<usize>().ok()).unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let tz = query.tz.and_then(|name| TimeZone::get(&name).ok()).unwrap_or_else(TimeZone::system);

    let projects = api.projects.project_roots().await;
    let now = jiff::Timestamp::now().as_millisecond();
    let list = api.reader.list(&projects, now).await;
    let mut response = Json(overview(list, now, &tz, limit)).into_response();
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}
