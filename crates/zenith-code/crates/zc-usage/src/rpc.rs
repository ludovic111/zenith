//! The RPC handlers (`ws.ts`): `server.getUsageSummary` and `server.refreshUsageRates`.

use serde_json::Value;
use zc_contracts::{ServerRefreshUsageRatesPayload, UsageSummaryInput};
use zc_rpc::{RpcError, RpcRouterBuilder};

use crate::service::UsageService;

fn decode<T: serde::de::DeserializeOwned>(payload: Value) -> Result<T, RpcError> {
    serde_json::from_value(payload).map_err(|error| RpcError::die_text(error.to_string()))
}

/// Registers both methods (scopes come from the router's scope table).
pub fn register(builder: RpcRouterBuilder, usage: UsageService) -> RpcRouterBuilder {
    let summary = usage.clone();
    builder
        .unary("server.getUsageSummary", move |_ctx, payload| {
            let usage = summary.clone();
            async move {
                let input: UsageSummaryInput = decode(payload)?;
                usage.read_summary(input).await.map_err(RpcError::fail)
            }
        })
        .unary("server.refreshUsageRates", move |_ctx, payload| {
            let usage = usage.clone();
            async move {
                decode::<ServerRefreshUsageRatesPayload>(payload)?;
                Ok(usage.refresh_rates().await)
            }
        })
}
