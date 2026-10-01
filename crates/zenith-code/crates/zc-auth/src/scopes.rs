//! Scopes (`packages/contracts/src/auth.ts`) and the per-RPC scope map
//! (`auth/RpcAuthorization.ts`), read from the generated method table.

use serde_json::Value;
use zc_contracts::{AuthEnvironmentScope as Scope, Rpc, METHODS};
use zc_rpc::{MethodOptions, ScopeRule, ScopeTable};

/// `AuthStandardClientScopes`: what a paired client gets by default.
pub const STANDARD_CLIENT_SCOPES: &[Scope] = &[
    Scope::OrchestrationRead,
    Scope::OrchestrationOperate,
    Scope::TerminalOperate,
    Scope::ReviewWrite,
    Scope::RelayRead,
];

/// `AuthAdministrativeScopes`: the owner's scopes (standard plus access and relay management).
pub const ADMINISTRATIVE_SCOPES: &[Scope] = &[
    Scope::OrchestrationRead,
    Scope::OrchestrationOperate,
    Scope::TerminalOperate,
    Scope::ReviewWrite,
    Scope::RelayRead,
    Scope::AccessRead,
    Scope::AccessWrite,
    Scope::RelayWrite,
];

/// A scope from its wire string.
pub fn parse_scope(value: &str) -> Option<Scope> {
    Scope::ALL.iter().copied().find(|scope| scope.as_str() == value)
}

/// Scopes from their wire strings; `None` if one is unknown (the TS literal decode fails).
pub fn parse_scopes<S: AsRef<str>>(values: &[S]) -> Option<Vec<Scope>> {
    values.iter().map(|value| parse_scope(value.as_ref())).collect()
}

/// The wire strings of `scopes`.
pub fn scope_strings(scopes: &[Scope]) -> Vec<String> {
    scopes.iter().map(|scope| scope.as_str().to_owned()).collect()
}

/// `RPC_REQUIRED_SCOPES` as a [`ScopeTable`] for [`zc_rpc::RpcRouterBuilder::scopes`].
pub fn rpc_scope_table() -> ScopeTable {
    METHODS.iter().map(|spec| (spec.tag, spec.scope.as_str())).collect()
}

/// `requiredScopeForRpcMethod`: the scope a method needs, or `None` for an unknown tag.
pub fn required_scope_for_rpc_method(tag: &str) -> Option<Scope> {
    Rpc::from_tag(tag).map(|rpc| rpc.spec().scope)
}

/// JavaScript truthiness of a decoded JSON value.
fn js_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// `requiredScopeForDeviceList`: retrying a host or updating a tool can install or restart
/// tools, so it needs `orchestration:operate`; plain listing needs `orchestration:read`.
pub fn required_scope_for_device_list(payload: &Value) -> Scope {
    if js_truthy(payload.get("retryHostId")) || js_truthy(payload.get("updateTool")) {
        Scope::OrchestrationOperate
    } else {
        Scope::OrchestrationRead
    }
}

/// The options a method must be registered with for its scope check: `device.list` gets its
/// payload-dependent rule, every other method uses the table.
pub fn rpc_method_options(tag: &str) -> MethodOptions {
    if tag == Rpc::DeviceList.tag() {
        MethodOptions::default().scope(ScopeRule::dynamic(|payload| required_scope_for_device_list(payload).as_str().to_owned()))
    } else {
        MethodOptions::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // RpcAuthorization.test.ts
    #[test]
    fn declares_exactly_one_scope_for_every_rpc() {
        let table = rpc_scope_table();
        assert_eq!(METHODS.len(), 148);
        for spec in METHODS.iter() {
            assert_eq!(table.get(spec.tag), Some(spec.scope.as_str()), "{}", spec.tag);
        }
    }

    #[test]
    fn background_policy_scopes_are_deliberate() {
        let s = |tag| required_scope_for_rpc_method(tag).unwrap();
        assert_eq!(s("server.reportClientActivity"), Scope::OrchestrationRead);
        assert_eq!(s("server.reportHostPowerState"), Scope::OrchestrationOperate);
        assert_eq!(s("server.getBackgroundPolicy"), Scope::OrchestrationRead);
        assert_eq!(s("subscribeBackgroundPolicy"), Scope::OrchestrationRead);
        assert_eq!(s("subscribeAuthAccess"), Scope::AccessRead);
    }

    #[test]
    fn relay_status_reads_do_not_grant_installation() {
        let s = |tag| required_scope_for_rpc_method(tag).unwrap();
        assert_eq!(s("cloud.getRelayClientStatus"), Scope::RelayRead);
        assert_eq!(s("cloud.installRelayClient"), Scope::RelayWrite);
        assert_eq!(s("provider.uploadFeedback"), Scope::OrchestrationOperate);
        assert_eq!(s("agentSessions.scan"), Scope::OrchestrationRead);
        assert_eq!(s("agentSessions.import"), Scope::OrchestrationOperate);
        assert_eq!(s("pullRequests.reviewerCandidates"), s("pullRequests.detail"));
        assert_eq!(s("pullRequests.requestReviewers"), s("pullRequests.comment"));
    }

    #[test]
    fn rejects_unknown_rpc_method_names() {
        for tag in ["server.notRegistered", "toString", "constructor"] {
            assert_eq!(required_scope_for_rpc_method(tag), None);
        }
    }

    #[test]
    fn device_list_needs_operate_for_retries_and_tool_updates() {
        assert_eq!(required_scope_for_device_list(&json!({})), Scope::OrchestrationRead);
        assert_eq!(
            required_scope_for_device_list(&json!({"retryHostId": "remote-host"})),
            Scope::OrchestrationOperate
        );
        assert_eq!(
            required_scope_for_device_list(&json!({"updateTool": "agent", "inspectOnly": true})),
            Scope::OrchestrationOperate
        );
        assert_eq!(required_scope_for_device_list(&json!({"updateTool": "hub"})), Scope::OrchestrationOperate);
        assert_eq!(required_scope_for_device_list(&json!({"retryHostId": ""})), Scope::OrchestrationRead);
    }

    #[test]
    fn scope_lists() {
        assert_eq!(
            scope_strings(ADMINISTRATIVE_SCOPES).join(" "),
            "orchestration:read orchestration:operate terminal:operate review:write relay:read access:read access:write relay:write"
        );
        assert_eq!(parse_scopes(&["access:read", "nope"]), None);
    }
}
