//! Who is calling: the session behind a socket, and the request being served.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::message::RequestId;

/// The authenticated session of a socket, produced by the `/ws` upgrade's auth hook.
///
/// `scopes` drives the per-method scope check. The auth crate can attach its own
/// session type in `extensions` (any `Clone + Send + Sync` value) and read it back in
/// handlers through [`RequestContext::auth`].
#[derive(Clone, Debug, Default)]
pub struct AuthContext {
    pub session_id: Option<String>,
    pub subject: Option<String>,
    pub scopes: BTreeSet<String>,
    pub extensions: http::Extensions,
}

impl AuthContext {
    pub fn new(scopes: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            scopes: scopes.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }

    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.contains(scope)
    }
}

/// One WebSocket connection, as handlers see it.
#[derive(Debug)]
pub struct ConnectionInfo {
    /// Unique per server process.
    pub id: u64,
    pub auth: AuthContext,
    /// Upgrade query parameters worth keeping (`clientSurface`, `clientAppVersion`, …).
    pub metadata: BTreeMap<String, String>,
    /// Anything else the HTTP layer wants handlers to see.
    pub extensions: http::Extensions,
}

/// What a handler gets besides its payload.
#[derive(Clone, Debug)]
pub struct RequestContext {
    pub connection: Arc<ConnectionInfo>,
    pub request_id: RequestId,
    pub tag: Arc<str>,
    /// Cancelled when the client interrupts the request, the socket closes or the
    /// server shuts down. The runtime also drops the handler's future then, so
    /// handlers only need it for work they spawn themselves.
    pub cancel: CancellationToken,
}

impl RequestContext {
    pub fn auth(&self) -> &AuthContext {
        &self.connection.auth
    }
}
