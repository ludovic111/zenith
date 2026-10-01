//! The background policy port (`apps/server/src/background/BackgroundPolicy.ts`).
//!
//! Per-client activity leases (at most 16 per RPC client, TTL 45 s by default, 120 s max) plus
//! the host power state decide whether pollers may run. Implemented in zc-orchestration
//! (`background/`); consumed by the VCS status broadcaster, provider status refresh, diagnostics,
//! thread pollers and the `server.reportClientActivity|reportHostPowerState|getBackgroundPolicy`
//! / `subscribeBackgroundPolicy` RPCs.

use async_trait::async_trait;

use crate::contracts::{AuthSessionId, BackgroundPolicySnapshot, BackgroundScope, ClientActivityReportInput, HostPowerSnapshot, RpcClientId};
use crate::EventStream;

/// `{latest, changes}` from `subscribe`: the snapshot as of subscription and every later one,
/// with nothing lost or repeated in between.
pub struct BackgroundPolicySubscription {
    pub latest: BackgroundPolicySnapshot,
    pub changes: EventStream<BackgroundPolicySnapshot>,
}

#[async_trait]
pub trait BackgroundPolicy: Send + Sync {
    /// `reportClientActivity(sessionId, rpcClientId, input)`: replace that client's leases.
    async fn report_client_activity(&self, session_id: &AuthSessionId, rpc_client_id: &RpcClientId, input: ClientActivityReportInput);

    /// `removeRpcClient(sessionId, rpcClientId)`: drop the leases of a closed socket.
    async fn remove_rpc_client(&self, session_id: &AuthSessionId, rpc_client_id: &RpcClientId);

    /// `reportHostPowerState(snapshot)`.
    async fn report_host_power_state(&self, snapshot: HostPowerSnapshot);

    /// `snapshot`.
    async fn snapshot(&self) -> BackgroundPolicySnapshot;

    /// `subscribe`: snapshot plus changes, atomically.
    async fn subscribe(&self) -> BackgroundPolicySubscription;

    /// `hasDemand(scope)`: some client currently wants this scope.
    async fn has_demand(&self, scope: &BackgroundScope) -> bool;

    /// `shouldRunScopeWork(scope)`: demand and power policy both allow polling this scope now.
    async fn should_run_scope_work(&self, scope: &BackgroundScope) -> bool;

    /// `shouldRunOpportunisticWork`: work nobody asked for (prefetch, warm caches) may run.
    async fn should_run_opportunistic_work(&self) -> bool;
}
