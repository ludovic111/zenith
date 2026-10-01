//! The WS RPC handlers this crate backs (`ws.ts`), registered on a [`RpcRouterBuilder`] with
//! their scopes (`RpcAuthorization.ts`):
//!
//! | Method | Kind | Scope | Behaviour |
//! |---|---|---|---|
//! | `orchestration.getTurnDiff` | unary | `orchestration:read` | [`CheckpointDiffQuery::get_turn_diff`]; failures become `OrchestrationGetTurnDiffError{message: "Failed to load turn diff", cause}` |
//! | `orchestration.getFullThreadDiff` | unary | `orchestration:read` | [`CheckpointDiffQuery::get_full_thread_diff`]; `OrchestrationGetFullThreadDiffError` |
//! | `subscribeWorktreeSetup` | stream | `orchestration:read` | [`WorktreeSetupTracker::stream`] |
//! | `worktreeSetup.cancel` | unary | `orchestration:operate` | [`WorktreeSetupTracker::cancel`] → `{cancelled}` |
//! | `server.reportClientActivity` | unary | `orchestration:read` | leases keyed by the socket's session and connection id |
//! | `server.reportHostPowerState` | unary | `orchestration:operate` | the host monitor's `report` |
//! | `server.getBackgroundPolicy` | unary | `orchestration:read` | the policy snapshot |
//! | `subscribeBackgroundPolicy` | stream | `orchestration:read` | the snapshot, then every change |
//!
//! A payload that does not decode dies with the decode error text, like the TS server's schema
//! decode failure. When a socket closes, the server calls
//! [`BackgroundRpc::connection_closed`] (the TS `removeRpcClient` finalizer of each socket).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use zc_contracts::{
    AuthSessionId, ClientActivityReportInput, HostPowerSnapshot, LitOrchestrationGetFullThreadDiffError, LitOrchestrationGetTurnDiffError,
    OrchestrationGetFullThreadDiffError, OrchestrationGetFullThreadDiffInput, OrchestrationGetTurnDiffError, OrchestrationGetTurnDiffInput, RpcClientId,
    WorktreeSetupCancelInput, WorktreeSetupCancelResult, WorktreeSetupSubscribeInput,
};
use zc_rpc::{MethodOptions, RpcError, RpcRouterBuilder, ScopeRule};

use crate::background::BackgroundPolicyService;
use crate::diff_query::CheckpointDiffQuery;
use crate::worktree_setup::WorktreeSetupTracker;

pub const ORCHESTRATION_GET_TURN_DIFF: &str = "orchestration.getTurnDiff";
pub const ORCHESTRATION_GET_FULL_THREAD_DIFF: &str = "orchestration.getFullThreadDiff";
pub const SUBSCRIBE_WORKTREE_SETUP: &str = "subscribeWorktreeSetup";
pub const WORKTREE_SETUP_CANCEL: &str = "worktreeSetup.cancel";
pub const SERVER_REPORT_CLIENT_ACTIVITY: &str = "server.reportClientActivity";
pub const SERVER_REPORT_HOST_POWER_STATE: &str = "server.reportHostPowerState";
pub const SERVER_GET_BACKGROUND_POLICY: &str = "server.getBackgroundPolicy";
pub const SUBSCRIBE_BACKGROUND_POLICY: &str = "subscribeBackgroundPolicy";

const ORCHESTRATION_READ: &str = "orchestration:read";
const ORCHESTRATION_OPERATE: &str = "orchestration:operate";

/// Every method of this module with its required scope.
pub const METHOD_SCOPES: [(&str, &str); 8] = [
    (ORCHESTRATION_GET_TURN_DIFF, ORCHESTRATION_READ),
    (ORCHESTRATION_GET_FULL_THREAD_DIFF, ORCHESTRATION_READ),
    (SUBSCRIBE_WORKTREE_SETUP, ORCHESTRATION_READ),
    (WORKTREE_SETUP_CANCEL, ORCHESTRATION_OPERATE),
    (SERVER_REPORT_CLIENT_ACTIVITY, ORCHESTRATION_READ),
    (SERVER_REPORT_HOST_POWER_STATE, ORCHESTRATION_OPERATE),
    (SERVER_GET_BACKGROUND_POLICY, ORCHESTRATION_READ),
    (SUBSCRIBE_BACKGROUND_POLICY, ORCHESTRATION_READ),
];

fn decode<T: DeserializeOwned>(payload: Value) -> Result<T, RpcError> {
    serde_json::from_value(payload).map_err(|error| RpcError::die_text(error.to_string()))
}

fn encode<T: Serialize>(value: &T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|e| RpcError::die(format!("could not encode the result: {e}")))
}

fn options(scope: &str) -> MethodOptions {
    MethodOptions::default().scope(ScopeRule::required(scope))
}

/// Per connection: its session and the rpc client ids it reported activity for.
type ConnectionClients = HashMap<u64, (AuthSessionId, HashSet<i64>)>;

/// The background-policy side of the socket handlers: which rpc client ids each connection
/// reported, so their leases go when the socket does.
#[derive(Clone)]
pub struct BackgroundRpc {
    policy: BackgroundPolicyService,
    clients: Arc<Mutex<ConnectionClients>>,
}

impl BackgroundRpc {
    pub fn new(policy: BackgroundPolicyService) -> Self {
        Self {
            policy,
            clients: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The socket closed: drop the leases of every rpc client it reported for.
    pub async fn connection_closed(&self, connection_id: u64) {
        let entry = self.clients.lock().unwrap().remove(&connection_id);
        if let Some((session_id, client_ids)) = entry {
            for client_id in client_ids {
                self.policy.remove_rpc_client(&session_id, RpcClientId(client_id)).await;
            }
        }
    }

    /// `server.reportClientActivity` for the socket `connection_id` of `session_id`.
    pub async fn report_client_activity(&self, session_id: &AuthSessionId, connection_id: u64, input: ClientActivityReportInput) {
        // The Effect RPC client id of the socket: one per connection.
        let rpc_client_id = i64::try_from(connection_id).unwrap_or(i64::MAX);
        self.clients
            .lock()
            .unwrap()
            .entry(connection_id)
            .or_insert_with(|| (session_id.clone(), HashSet::new()))
            .1
            .insert(rpc_client_id);
        self.policy.report_client_activity(session_id, RpcClientId(rpc_client_id), input).await;
    }

    /// The policy behind the handlers.
    pub fn policy(&self) -> &BackgroundPolicyService {
        &self.policy
    }
}

/// What the handlers need.
#[derive(Clone)]
pub struct CheckpointRpcServices {
    pub diff_query: CheckpointDiffQuery,
    pub worktree_setup: WorktreeSetupTracker,
    pub background: BackgroundRpc,
}

/// `orchestration.getTurnDiff`.
pub async fn get_turn_diff(services: &CheckpointRpcServices, payload: Value) -> Result<Value, RpcError> {
    let input: OrchestrationGetTurnDiffInput = decode(payload)?;
    match services.diff_query.get_turn_diff(&input).await {
        Ok(diff) => encode(&diff),
        Err(error) => Err(RpcError::fail(OrchestrationGetTurnDiffError {
            tag: LitOrchestrationGetTurnDiffError,
            message: "Failed to load turn diff".into(),
            cause: Some(error.to_defect().0),
        })),
    }
}

/// `orchestration.getFullThreadDiff`.
pub async fn get_full_thread_diff(services: &CheckpointRpcServices, payload: Value) -> Result<Value, RpcError> {
    let input: OrchestrationGetFullThreadDiffInput = decode(payload)?;
    match services.diff_query.get_full_thread_diff(&input).await {
        Ok(diff) => encode(&diff),
        Err(error) => Err(RpcError::fail(OrchestrationGetFullThreadDiffError {
            tag: LitOrchestrationGetFullThreadDiffError,
            message: "Failed to load full thread diff".into(),
            cause: Some(error.to_defect().0),
        })),
    }
}

/// `worktreeSetup.cancel`.
pub async fn worktree_setup_cancel(services: &CheckpointRpcServices, payload: Value) -> Result<Value, RpcError> {
    let input: WorktreeSetupCancelInput = decode(payload)?;
    let cancelled = services.worktree_setup.cancel(&input.thread_id).await;
    encode(&WorktreeSetupCancelResult { cancelled })
}

/// Adds every method of the table above.
pub fn register(builder: RpcRouterBuilder, services: CheckpointRpcServices) -> RpcRouterBuilder {
    let s = services.clone();
    let builder = builder.unary_with(ORCHESTRATION_GET_TURN_DIFF, options(ORCHESTRATION_READ), move |_ctx, payload| {
        let s = s.clone();
        async move { get_turn_diff(&s, payload).await }
    });
    let s = services.clone();
    let builder = builder.unary_with(ORCHESTRATION_GET_FULL_THREAD_DIFF, options(ORCHESTRATION_READ), move |_ctx, payload| {
        let s = s.clone();
        async move { get_full_thread_diff(&s, payload).await }
    });
    let s = services.clone();
    let builder = builder.stream_with(SUBSCRIBE_WORKTREE_SETUP, options(ORCHESTRATION_READ), move |_ctx, payload| {
        let s = s.clone();
        async move {
            let input: WorktreeSetupSubscribeInput = decode(payload)?;
            Ok(s.worktree_setup.stream(&input.thread_id).map(|snapshot| encode(&snapshot)))
        }
    });
    let s = services.clone();
    let builder = builder.unary_with(WORKTREE_SETUP_CANCEL, options(ORCHESTRATION_OPERATE), move |_ctx, payload| {
        let s = s.clone();
        async move { worktree_setup_cancel(&s, payload).await }
    });
    let s = services.clone();
    let builder = builder.unary_with(SERVER_REPORT_CLIENT_ACTIVITY, options(ORCHESTRATION_READ), move |ctx, payload| {
        let s = s.clone();
        async move {
            let input: ClientActivityReportInput = decode(payload)?;
            let session_id = AuthSessionId::new(ctx.auth().session_id.clone().unwrap_or_default());
            s.background.report_client_activity(&session_id, ctx.connection.id, input).await;
            Ok(Value::Null)
        }
    });
    let s = services.clone();
    let builder = builder.unary_with(SERVER_REPORT_HOST_POWER_STATE, options(ORCHESTRATION_OPERATE), move |_ctx, payload| {
        let s = s.clone();
        async move {
            let input: HostPowerSnapshot = decode(payload)?;
            s.background.policy.report_host_power_state(input).await;
            Ok(Value::Null)
        }
    });
    let s = services.clone();
    let builder = builder.unary_with(SERVER_GET_BACKGROUND_POLICY, options(ORCHESTRATION_READ), move |_ctx, _payload| {
        let s = s.clone();
        async move { encode(&s.background.policy.snapshot().await) }
    });
    let s = services;
    builder.stream_with(SUBSCRIBE_BACKGROUND_POLICY, options(ORCHESTRATION_READ), move |_ctx, _payload| {
        let s = s.clone();
        async move {
            let (latest, changes) = s.background.policy.subscribe().await;
            Ok(futures::stream::once(async move { latest }).chain(changes).map(|snapshot| encode(&snapshot)))
        }
    })
}
