//! The typed-route authentication hooks of the other crates, over zc-auth's
//! `EnvironmentAuthenticatedAuth` (session cookie, then `Authorization: Bearer`, then DPoP;
//! plan §2.4).

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::ConnectInfo;
use axum::http::request::Parts;
use zc_auth::{AuthRequest, AuthenticatedSession, EnvironmentAuth};
use zc_http::EnvironmentError;
use zc_rpc::AuthContext;

/// One authenticator for every typed route that is not zc-auth's own.
#[derive(Clone, Debug)]
pub struct HttpAuth(pub Arc<EnvironmentAuth>);

impl HttpAuth {
    /// The session behind a request, or the 401/500 to answer.
    pub async fn session(&self, request: &AuthRequest<'_>) -> Result<AuthenticatedSession, EnvironmentError> {
        match self.0.authenticate_request(request).await {
            Ok(session) => Ok(session),
            Err(error) if error.is_credential_error() => Err(EnvironmentError::AuthInvalid {
                reason: error.credential_reason().to_owned(),
                dpop_failure_reason: error.dpop_failure_reason().map(|r| r.as_str().to_owned()),
                trace_id: None,
            }),
            Err(error) => {
                tracing::error!(tag = error.tag(), %error, "environment api operation failed");
                Err(EnvironmentError::internal("internal_error"))
            }
        }
    }

    pub async fn session_of_parts(&self, parts: &Parts) -> Result<AuthenticatedSession, EnvironmentError> {
        self.session(&zc_auth::http::auth_request(parts)).await
    }
}

#[async_trait]
impl zc_sessions::HttpAuthenticator for HttpAuth {
    async fn authenticate(&self, request: &zc_sessions::HttpAuthRequest<'_>) -> Result<AuthContext, EnvironmentError> {
        let path_and_query = request.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
        let view = AuthRequest {
            method: "GET",
            headers: request.headers,
            path_and_query,
            peer: request.peer,
        };
        self.session(&view).await.map(zc_auth::ws::auth_context)
    }
}

#[async_trait]
impl zc_projections::http::EnvironmentAuth for HttpAuth {
    async fn authenticate(&self, parts: &Parts) -> Result<Vec<String>, EnvironmentError> {
        let session = self.session_of_parts(parts).await?;
        Ok(session.scopes.iter().map(|scope| scope.as_str().to_owned()).collect())
    }
}

/// The peer address axum recorded for a request.
pub fn peer(parts: &Parts) -> Option<SocketAddr> {
    parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0)
}
