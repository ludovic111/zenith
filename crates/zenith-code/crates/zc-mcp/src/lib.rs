//! zc-mcp: the `t3-code` MCP server the agents call back into (WP-27 of
//! `docs/zenith-code-rust-plan.md`, §6.9), ported from `apps/server/src/mcp/**`.
//!
//! - [`registry`]: [`McpSessionRegistry`] (`McpSessionRegistry.ts` + `McpProviderSession.ts`):
//!   one bearer credential per provider session (32 random bytes, base64url; only the SHA-256
//!   is kept, in memory), a 24 h liveness window refreshed by MCP traffic and by every provider
//!   turn, revocation per provider session, per thread or all, and the per-thread
//!   [`McpProviderSessionConfig`] the drivers read to hand the `t3-code` server to Claude and
//!   Codex. It implements the provider service's [`zc_providers::hooks::McpSessions`] hook.
//! - [`scope`]: [`McpInvocationScope`] (`McpInvocationContext.ts`) and the capability check.
//! - [`broker`]: [`PreviewAutomationBroker`] (`PreviewAutomationBroker.ts`): routes preview tool
//!   calls to a desktop browser host connected over `previewAutomation.connect`, correlates the
//!   answers from `previewAutomation.respond`, pins a provider session to one host.
//! - [`tools`]: the three toolkits (pull requests, preview, device) behind one [`tools::Toolkit`].
//!   The tool descriptors are the TypeScript server's own `tools/list` output (`src/tools.json`,
//!   written by `code/apps/server/scripts/mcp-oracle.ts tools`), served unchanged.
//! - [`http`]: `POST`/`DELETE /mcp`, Streamable HTTP for protocol `2025-06-18` as Effect's
//!   `McpServer.layerHttp` serves it (with the local patch adding `DELETE`), behind the
//!   bearer check of `McpHttpServer.ts`.
//! - [`rpc`]: the `previewAutomation.*` RPC methods.

pub mod broker;
pub mod errors;
pub mod http;
pub mod params;
pub mod provider_session;
pub mod registry;
pub mod rpc;
pub mod scope;
pub mod tools;

pub use broker::{PreviewAutomationBroker, PreviewAutomationInvokeInput};
pub use http::{router, McpHttpOptions};
pub use provider_session::{with_agent_device_environment, McpProviderSessionConfig};
pub use registry::{McpCredentialRequest, McpIssuedCredential, McpSessionRegistry, McpSessionRegistryOptions};
pub use scope::{require_capability, McpCapability, McpInvocationScope};
pub use tools::{McpServices, OrchestrationPullRequests, PullRequestBackend, Toolkit};

/// The product name the build writes where upstream says "T3 Code" (`scripts/lib/zenith-brand.ts`
/// rewrites every first-party server string, the MCP server name and the tool descriptions too).
pub const BRAND_NAME: &str = "zenith";

/// `zenithBrandPlugin`'s rewrite of one string.
pub fn brand(text: &str) -> String {
    text.replace("T3 Code", BRAND_NAME)
}
