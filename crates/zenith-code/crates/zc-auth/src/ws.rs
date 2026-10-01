//! The `/ws` side of auth: [`zc_http::WsAuthenticator`] over [`EnvironmentAuth`]
//! (`ws.ts` `websocketRpcRouteLayer`), and the `subscribeAuthAccess` stream
//! (`ws.ts:384-421`, `3747-3780`).

use std::sync::Arc;

use async_trait::async_trait;
use futures::{stream, Stream, StreamExt};
use serde_json::Value;
use zc_contracts::{
    AuthAccessSnapshot, AuthAccessStreamClientRemovedEvent, AuthAccessStreamClientRemovedEventPayload, AuthAccessStreamClientUpsertedEvent,
    AuthAccessStreamError, AuthAccessStreamEvent, AuthAccessStreamPairingLinkRemovedEvent, AuthAccessStreamPairingLinkRemovedEventPayload,
    AuthAccessStreamPairingLinkUpsertedEvent, AuthAccessStreamSnapshotEvent, AuthSessionId, JsNumber, Lit1, LitAuthAccessStreamError, LitClientRemoved,
    LitClientUpserted, LitPairingLinkRemoved, LitPairingLinkUpserted, LitSnapshot, Rpc,
};
use zc_http::{EnvironmentError, UpgradeRequest, WsAuthenticator, WsQuery};
use zc_rpc::{AuthContext, RequestContext, RpcError, RpcRouterBuilder};

use crate::environment_auth::{AuthRequest, AuthenticatedSession, EnvironmentAuth};
use crate::pairing::BootstrapCredentialChange;
use crate::scopes::rpc_method_options;
use crate::session_store::SessionCredentialChange;

/// Authenticates `/ws` upgrades and tracks connected sessions.
#[derive(Clone, Debug)]
pub struct AuthWsAuthenticator {
    auth: Arc<EnvironmentAuth>,
}

impl AuthWsAuthenticator {
    pub fn new(auth: Arc<EnvironmentAuth>) -> Self {
        Self { auth }
    }
}

/// The RPC view of a session: scopes as strings for the per-method check, the session itself
/// in the extensions for handlers that need more.
pub fn auth_context(session: AuthenticatedSession) -> AuthContext {
    let mut context = AuthContext::new(session.scopes.iter().map(|s| s.as_str()));
    context.session_id = Some(session.session_id.clone());
    context.subject = Some(session.subject.clone());
    context.extensions.insert(session);
    context
}

#[async_trait]
impl WsAuthenticator for AuthWsAuthenticator {
    async fn authenticate(&self, request: &UpgradeRequest<'_>) -> Result<AuthContext, EnvironmentError> {
        let view = AuthRequest {
            method: "GET",
            headers: request.headers,
            path_and_query: request.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/ws"),
            peer: request.peer,
        };
        match self.auth.authenticate_websocket_upgrade(&view, request.query.ws_ticket.as_deref()).await {
            Ok(session) => Ok(auth_context(session)),
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

    async fn connected(&self, auth: &AuthContext, query: &WsQuery) {
        let Some(session_id) = auth.session_id.as_deref() else {
            return;
        };
        let origin = query.client_origin();
        self.auth
            .sessions()
            .record_client_connection(session_id, origin.surface.as_deref(), origin.app_version.as_deref())
            .await;
        self.auth.sessions().mark_connected(session_id).await;
    }

    async fn disconnected(&self, auth: &AuthContext) {
        if let Some(session_id) = auth.session_id.as_deref() {
            self.auth.sessions().mark_disconnected(session_id).await;
        }
    }
}

/// One change of either feed.
#[derive(Clone, Debug)]
enum AccessChange {
    Pairing(BootstrapCredentialChange),
    Session(SessionCredentialChange),
}

fn to_event(change: AccessChange, revision: u64, current_session_id: &str) -> AuthAccessStreamEvent {
    let revision = JsNumber(revision as f64);
    match change {
        AccessChange::Pairing(BootstrapCredentialChange::PairingLinkUpserted(link)) => {
            AuthAccessStreamEvent::AuthAccessStreamPairingLinkUpsertedEvent(AuthAccessStreamPairingLinkUpsertedEvent {
                version: Lit1,
                revision,
                r#type: LitPairingLinkUpserted,
                payload: link,
            })
        }
        AccessChange::Pairing(BootstrapCredentialChange::PairingLinkRemoved(id)) => {
            AuthAccessStreamEvent::AuthAccessStreamPairingLinkRemovedEvent(AuthAccessStreamPairingLinkRemovedEvent {
                version: Lit1,
                revision,
                r#type: LitPairingLinkRemoved,
                payload: AuthAccessStreamPairingLinkRemovedEventPayload { id },
            })
        }
        AccessChange::Session(SessionCredentialChange::ClientUpserted(mut session)) => {
            session.current = session.session_id.as_str() == current_session_id;
            AuthAccessStreamEvent::AuthAccessStreamClientUpsertedEvent(AuthAccessStreamClientUpsertedEvent {
                version: Lit1,
                revision,
                r#type: LitClientUpserted,
                payload: *session,
            })
        }
        AccessChange::Session(SessionCredentialChange::ClientRemoved(session_id)) => {
            AuthAccessStreamEvent::AuthAccessStreamClientRemovedEvent(AuthAccessStreamClientRemovedEvent {
                version: Lit1,
                revision,
                r#type: LitClientRemoved,
                payload: AuthAccessStreamClientRemovedEventPayload {
                    session_id: AuthSessionId::new(session_id),
                },
            })
        }
    }
}

fn encode(event: &AuthAccessStreamEvent) -> Result<Value, RpcError> {
    serde_json::to_value(event).map_err(|e| RpcError::die(format!("could not encode the event: {e}")))
}

/// `subscribeAuthAccess`: a snapshot (`revision` 1) of the pairing links and client sessions,
/// then every change with the next revision. Both feeds are subscribed before the snapshot is
/// read, so nothing between the two is lost (an upsert may repeat what the snapshot shows).
pub async fn subscribe_auth_access(
    auth: &EnvironmentAuth,
    current_session_id: String,
) -> Result<impl Stream<Item = Result<Value, RpcError>> + Send + 'static, RpcError> {
    let pairing = auth.pairing().subscribe_changes().map(AccessChange::Pairing);
    let sessions = auth.sessions().subscribe_changes().map(AccessChange::Session);
    let snapshot_error = |message: String| {
        RpcError::fail(AuthAccessStreamError {
            tag: LitAuthAccessStreamError,
            message,
        })
    };
    let pairing_links = auth.list_pairing_links(None).await.map_err(|e| snapshot_error(e.to_string()))?;
    let client_sessions = auth
        .list_client_sessions(&current_session_id)
        .await
        .map_err(|e| snapshot_error(e.to_string()))?;
    let snapshot = encode(&AuthAccessStreamEvent::AuthAccessStreamSnapshotEvent(AuthAccessStreamSnapshotEvent {
        version: Lit1,
        revision: JsNumber(1.0),
        r#type: LitSnapshot,
        payload: AuthAccessSnapshot {
            pairing_links,
            client_sessions,
        },
    }))?;
    let live = stream::select(pairing, sessions).scan(1u64, move |revision, change| {
        *revision += 1;
        futures::future::ready(Some(encode(&to_event(change, *revision, &current_session_id))))
    });
    Ok(stream::once(futures::future::ready(Ok(snapshot))).chain(live))
}

/// Registers `subscribeAuthAccess` on an RPC router.
pub fn register_auth_access(builder: RpcRouterBuilder, auth: Arc<EnvironmentAuth>) -> RpcRouterBuilder {
    let tag = Rpc::SubscribeAuthAccess.tag();
    builder.stream_with(tag, rpc_method_options(tag), move |ctx: RequestContext, payload: Value| {
        let auth = auth.clone();
        async move {
            if !payload.is_object() {
                return Err(RpcError::die_text("Expected an object"));
            }
            let current = ctx.auth().session_id.clone().unwrap_or_default();
            subscribe_auth_access(&auth, current).await
        }
    })
}
