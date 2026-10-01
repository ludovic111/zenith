//! `GET /ws`: authenticate the upgrade, then hand the socket to [`zc_rpc::RpcServer`]
//! (`ws.ts` `websocketRpcRouteLayer`, plan §1.3 rule 11 and §2.4).
//!
//! Authentication is a trait ([`WsAuthenticator`]) that zc-auth implements: `?wsTicket=`
//! first, then cookie / bearer / DPoP. It fails with a typed [`EnvironmentError`] (401 or
//! 500), answered before any upgrade. It also gets the connect / disconnect events
//! (`markConnected` / `markDisconnected`, `recordClientConnection`).
//!
//! The socket is yawc's, not axum's: like the TS server (`websocket: { perMessageDeflate: true }`)
//! it negotiates `permessage-deflate` with clients that offer it (browsers do), which
//! tungstenite cannot.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use http::{header, HeaderMap, StatusCode, Uri};
use tokio::io::{AsyncRead, AsyncWrite};
use yawc::frame::{Frame, OpCode};
use yawc::{IncomingUpgrade, Options, WebSocket};
use zc_rpc::{AuthContext, ConnectionSetup, Inbound, Outbound, RpcServer};

use crate::error::EnvironmentError;

/// The upgrade's query parameters, read leniently: all optional, first value wins,
/// unknown ones ignored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WsQuery {
    pub ws_ticket: Option<String>,
    pub client_surface: Option<String>,
    pub client_app_version: Option<String>,
    pub client_device_type: Option<String>,
    pub client_os: Option<String>,
    pub client_web_deployment: Option<String>,
    pub client_browser: Option<String>,
    pub client_os_major_version: Option<String>,
    pub client_device_model: Option<String>,
    pub connection_method: Option<String>,
    /// `orchestrationProtocol=1`; ignored by the server.
    pub orchestration_protocol: Option<String>,
}

/// `metadata.origin` of dispatched events (`ws.ts` `readClientConnectionOrigin`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientOrigin {
    pub surface: Option<String>,
    pub app_version: Option<String>,
}

const CLIENT_SURFACES: &[&str] = &["web", "desktop", "mobile", "cli"];
const MAX_CLIENT_APP_VERSION_LENGTH: usize = 64;

impl WsQuery {
    pub fn parse(raw: Option<&str>) -> Self {
        let mut query = Self::default();
        let Some(raw) = raw else { return query };
        for (key, value) in url::form_urlencoded::parse(raw.as_bytes()) {
            let slot = match key.as_ref() {
                "wsTicket" => &mut query.ws_ticket,
                "clientSurface" => &mut query.client_surface,
                "clientAppVersion" => &mut query.client_app_version,
                "clientDeviceType" => &mut query.client_device_type,
                "clientOs" => &mut query.client_os,
                "clientWebDeployment" => &mut query.client_web_deployment,
                "clientBrowser" => &mut query.client_browser,
                "clientOsMajorVersion" => &mut query.client_os_major_version,
                "clientDeviceModel" => &mut query.client_device_model,
                "connectionMethod" => &mut query.connection_method,
                "orchestrationProtocol" => &mut query.orchestration_protocol,
                _ => continue,
            };
            if slot.is_none() {
                *slot = Some(value.into_owned());
            }
        }
        query
    }

    /// The validated surface and app version (invalid values are dropped, never fatal).
    pub fn client_origin(&self) -> ClientOrigin {
        let surface = self.client_surface.as_deref().filter(|s| CLIENT_SURFACES.contains(s)).map(str::to_owned);
        let app_version = self
            .client_app_version
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty() && v.chars().count() <= MAX_CLIENT_APP_VERSION_LENGTH)
            .map(str::to_owned);
        ClientOrigin { surface, app_version }
    }

    /// Everything that was given except the ticket, for [`zc_rpc::ConnectionInfo::metadata`].
    pub fn metadata(&self) -> BTreeMap<String, String> {
        let pairs = [
            ("clientSurface", &self.client_surface),
            ("clientAppVersion", &self.client_app_version),
            ("clientDeviceType", &self.client_device_type),
            ("clientOs", &self.client_os),
            ("clientWebDeployment", &self.client_web_deployment),
            ("clientBrowser", &self.client_browser),
            ("clientOsMajorVersion", &self.client_os_major_version),
            ("clientDeviceModel", &self.client_device_model),
            ("connectionMethod", &self.connection_method),
            ("orchestrationProtocol", &self.orchestration_protocol),
        ];
        pairs.into_iter().filter_map(|(k, v)| v.as_ref().map(|v| (k.to_owned(), v.clone()))).collect()
    }
}

/// What the authenticator sees of the upgrade request.
#[derive(Debug)]
pub struct UpgradeRequest<'a> {
    pub headers: &'a HeaderMap,
    pub uri: &'a Uri,
    pub query: &'a WsQuery,
    pub peer: Option<SocketAddr>,
}

/// The auth hook of `/ws`, implemented by zc-auth.
#[async_trait]
pub trait WsAuthenticator: Send + Sync + 'static {
    /// The session behind the upgrade, or the error to answer instead of upgrading
    /// (`EnvironmentAuthInvalidError` 401, `EnvironmentInternalError` 500).
    async fn authenticate(&self, request: &UpgradeRequest<'_>) -> Result<AuthContext, EnvironmentError>;

    /// The socket is open (`recordClientConnection` + `markConnected`).
    async fn connected(&self, _auth: &AuthContext, _query: &WsQuery) {}

    /// The socket is closed (`markDisconnected`).
    async fn disconnected(&self, _auth: &AuthContext) {}
}

/// Grants fixed scopes to every socket. For dev servers and tests only: never use it
/// where the port is reachable by anything but the developer.
#[derive(Clone, Debug)]
pub struct DevAuthenticator {
    pub scopes: Vec<String>,
}

#[async_trait]
impl WsAuthenticator for DevAuthenticator {
    async fn authenticate(&self, _request: &UpgradeRequest<'_>) -> Result<AuthContext, EnvironmentError> {
        let mut auth = AuthContext::new(self.scopes.iter().cloned());
        auth.subject = Some("dev".into());
        Ok(auth)
    }
}

/// Optional Rust-only hardening (plan §2.6): refuse upgrades whose `Origin` is neither
/// this server nor an allowed origin. Requests without `Origin` pass. Off by default,
/// since the TS server checks no Origin.
#[derive(Clone, Debug, Default)]
pub struct OriginCheck {
    pub allowed: Vec<String>,
}

impl OriginCheck {
    fn allows(&self, headers: &HeaderMap) -> bool {
        let Some(origin) = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()) else {
            return true;
        };
        if self.allowed.iter().any(|a| a == origin) {
            return true;
        }
        let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
        host.is_some_and(|host| origin == format!("http://{host}") || origin == format!("https://{host}"))
    }
}

/// The socket options: `permessage-deflate` with context takeover (the `ws` package's
/// defaults), UTF-8 checked text frames, and axum/tungstenite's message limits (64 MiB a
/// message, as `ws.ts` never needed more).
pub fn socket_options() -> Options {
    const MAX_MESSAGE: usize = 64 << 20;
    Options::default()
        .with_balanced_compression()
        .with_utf8()
        .with_no_delay()
        .with_limits(MAX_MESSAGE, 2 * MAX_MESSAGE)
}

/// State of the `/ws` route.
#[derive(Clone)]
pub struct WsState {
    pub rpc: Arc<RpcServer>,
    pub auth: Arc<dyn WsAuthenticator>,
    pub origin_check: Option<OriginCheck>,
}

/// The `/ws` handler: `.route("/ws", get(ws_upgrade)).with_state(ws_state)`.
pub async fn ws_upgrade(State(state): State<WsState>, request: Request) -> Response {
    let (mut parts, _body) = request.into_parts();
    if let Some(check) = &state.origin_check {
        if !check.allows(&parts.headers) {
            return (StatusCode::FORBIDDEN, "Forbidden origin").into_response();
        }
    }
    let query = WsQuery::parse(parts.uri.query());
    let peer = parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0);
    let auth = {
        let upgrade_request = UpgradeRequest {
            headers: &parts.headers,
            uri: &parts.uri,
            query: &query,
            peer,
        };
        match state.auth.authenticate(&upgrade_request).await {
            Ok(auth) => auth,
            Err(error) => return error.into_response(),
        }
    };
    if !is_upgrade_request(&parts.headers) {
        return (StatusCode::BAD_REQUEST, "Expected a WebSocket upgrade").into_response();
    }
    let upgrade = match IncomingUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(upgrade) => upgrade,
        Err(status) => return status.into_response(),
    };
    let (response, socket) = match upgrade.upgrade(socket_options()) {
        Ok(upgrade) => upgrade,
        Err(error) => {
            tracing::warn!(%error, "websocket upgrade failed");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    tokio::spawn(async move {
        let socket = match socket.await {
            Ok(socket) => socket,
            Err(error) => {
                tracing::debug!(%error, "websocket upgrade did not complete");
                return;
            }
        };
        state.auth.connected(&auth, &query).await;
        let setup = ConnectionSetup {
            auth: auth.clone(),
            metadata: query.metadata(),
            extensions: Default::default(),
        };
        serve_websocket(&state.rpc, setup, socket).await;
        state.auth.disconnected(&auth).await;
    });
    response.into_response()
}

/// `Connection: upgrade` and `Upgrade: websocket` (what axum's extractor checked; yawc's
/// only checks the key and the version).
fn is_upgrade_request(headers: &HeaderMap) -> bool {
    let has_token = |name: header::HeaderName, token: &str| {
        headers
            .get_all(name)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|part| part.trim().eq_ignore_ascii_case(token))
    };
    has_token(header::CONNECTION, "upgrade") && has_token(header::UPGRADE, "websocket")
}

/// Runs the RPC protocol on an upgraded socket. Pings are answered and close frames echoed
/// by the socket itself.
pub async fn serve_websocket<S>(rpc: &Arc<RpcServer>, setup: ConnectionSetup, socket: WebSocket<S>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (sink, stream) = socket.split();
    // The stream ends on a read error, which the RPC server treats as a close.
    let inbound = stream
        .filter_map(|frame| async move {
            match frame.opcode() {
                OpCode::Text => Some(Inbound::Text(String::from_utf8_lossy(frame.payload()).into_owned())),
                OpCode::Binary => Some(Inbound::Binary(frame.payload().to_vec())),
                OpCode::Close => Some(Inbound::Close),
                _ => None,
            }
        })
        .boxed();
    let outbound = sink.with(|frame: Outbound| async move {
        Ok::<_, yawc::WebSocketError>(match frame {
            Outbound::Text(text) => Frame::text(text),
            Outbound::Close { code, reason } => Frame::close(code.into(), reason),
        })
    });
    rpc.serve_socket(setup, inbound, Box::pin(outbound)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_is_lenient() {
        let q = WsQuery::parse(Some(
            "wsTicket=abc&clientSurface=web&clientAppVersion=%201.2.3%20&clientSurface=cli&x=1&orchestrationProtocol=1",
        ));
        assert_eq!(q.ws_ticket.as_deref(), Some("abc"));
        assert_eq!(q.client_surface.as_deref(), Some("web"));
        assert_eq!(
            q.client_origin(),
            ClientOrigin {
                surface: Some("web".into()),
                app_version: Some("1.2.3".into())
            }
        );
        assert!(!q.metadata().contains_key("wsTicket"));
        let bad = WsQuery::parse(Some("clientSurface=toaster&clientAppVersion=%20"));
        assert_eq!(bad.client_origin(), ClientOrigin::default());
        assert_eq!(WsQuery::parse(None), WsQuery::default());
    }

    #[test]
    fn upgrade_headers() {
        let mut h = HeaderMap::new();
        assert!(!is_upgrade_request(&h));
        h.insert(header::CONNECTION, "keep-alive, Upgrade".parse().unwrap());
        assert!(!is_upgrade_request(&h));
        h.insert(header::UPGRADE, "WebSocket".parse().unwrap());
        assert!(is_upgrade_request(&h));
        h.insert(header::UPGRADE, "h2c".parse().unwrap());
        assert!(!is_upgrade_request(&h));
    }

    #[test]
    fn origin_check() {
        let check = OriginCheck {
            allowed: vec!["http://127.0.0.1:4747".into()],
        };
        let mut h = HeaderMap::new();
        assert!(check.allows(&h));
        h.insert(header::HOST, "127.0.0.1:3773".parse().unwrap());
        h.insert(header::ORIGIN, "http://127.0.0.1:3773".parse().unwrap());
        assert!(check.allows(&h));
        h.insert(header::ORIGIN, "http://127.0.0.1:4747".parse().unwrap());
        assert!(check.allows(&h));
        h.insert(header::ORIGIN, "https://evil.test".parse().unwrap());
        assert!(!check.allows(&h));
    }
}
