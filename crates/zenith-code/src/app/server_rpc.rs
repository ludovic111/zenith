//! The `server.*` odds and ends of `ws.ts` this crate owns, and the stubs of the features zenith
//! does not run (plan §6.13–6.14):
//!
//! | Method / route | Answer |
//! |---|---|
//! | `server.probe` | `{}` (the connection probe) |
//! | `subscribeServerLifecycle` | latest `welcome` / `ready`, then live events |
//! | `cloud.getRelayClientStatus` | `{"status":"missing","version":"2026.5.2"}` (T3 Connect is inert) |
//! | `cloud.installRelayClient` | fails with `RelayClientInstallFailedError` (`unsupported_platform`) |
//! | `subscribeDeviceState`, `device.list` | an empty, disabled `DeviceServiceState` (devices are WP-32) |
//! | `GET /api/connect/link-state` | `relay:read`, then "not linked" |

use std::sync::Arc;

use axum::extract::State;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures::stream::{self, StreamExt};
use serde_json::{json, Value};
use zc_rpc::{RpcError, RpcRouterBuilder};

use super::http_auth::HttpAuth;
use super::lifecycle::ServerLifecycleEvents;

/// `CLOUDFLARED_VERSION`: the relay client the TS server would install.
pub const RELAY_CLIENT_VERSION: &str = "2026.5.2";

/// An empty device service: no host, nothing enabled.
pub fn empty_device_state() -> Value {
    json!({
        "hosts": [],
        "hostStatus": "disabled",
        "hostStatuses": {},
        "devices": [],
        "sessions": [],
        "onboardingCompleted": false,
        "agentAccessEnabled": false,
        "hubBasePath": "/api/device-hub",
        "revision": 0,
    })
}

/// `EnvironmentCloudLinkStateResult` of an environment that was never linked.
pub fn unlinked_cloud_state() -> Value {
    json!({
        "linked": false,
        "cloudUserId": null,
        "relayUrl": null,
        "relayIssuer": null,
        "managedTunnelActive": false,
        "publishAgentActivity": false,
    })
}

fn require_object(payload: &Value) -> Result<(), RpcError> {
    if payload.is_object() {
        Ok(())
    } else {
        Err(RpcError::die_text("Expected an object"))
    }
}

/// Registers the methods of the table above.
pub fn register(builder: RpcRouterBuilder, lifecycle: ServerLifecycleEvents) -> RpcRouterBuilder {
    builder
        .unary("server.probe", |_ctx, payload| async move {
            require_object(&payload)?;
            Ok(json!({}))
        })
        .stream("subscribeServerLifecycle", move |_ctx, payload| {
            let lifecycle = lifecycle.clone();
            async move {
                require_object(&payload)?;
                Ok(lifecycle.subscribe().map(Ok))
            }
        })
        .unary("cloud.getRelayClientStatus", |_ctx, payload| async move {
            require_object(&payload)?;
            Ok(json!({ "status": "missing", "version": RELAY_CLIENT_VERSION }))
        })
        .stream("cloud.installRelayClient", |_ctx, payload| async move {
            require_object(&payload)?;
            Err::<stream::Empty<Result<Value, RpcError>>, _>(RpcError::fail(json!({
                "_tag": "RelayClientInstallFailedError",
                "reason": "unsupported_platform",
                "message": "T3 Connect is not available in zenith.",
            })))
        })
        .stream("subscribeDeviceState", |_ctx, payload| async move {
            require_object(&payload)?;
            Ok(stream::once(async { Ok(empty_device_state()) }).chain(stream::pending()))
        })
        .unary_with("device.list", zc_auth::rpc_method_options("device.list"), |_ctx, payload| async move {
            require_object(&payload)?;
            Ok(empty_device_state())
        })
}

async fn link_state(State(auth): State<HttpAuth>, parts: Parts) -> Response {
    let session = match auth.session_of_parts(&parts).await {
        Ok(session) => session,
        Err(error) => return error.into_response(),
    };
    if !session.scopes.iter().any(|scope| scope.as_str() == "relay:read") {
        return zc_http::EnvironmentError::scope_required("relay:read").into_response();
    }
    Json(unlinked_cloud_state()).into_response()
}

/// `GET /api/connect/link-state`.
pub fn routes(auth: Arc<zc_auth::EnvironmentAuth>) -> Router {
    Router::new().route("/api/connect/link-state", get(link_state)).with_state(HttpAuth(auth))
}
