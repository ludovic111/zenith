//! WP-27, the `t3-code` MCP server (zc-mcp) in the running server.
//!
//! - [`McpPlugin`]: `POST`/`DELETE /mcp` (bearer credentials from [`AppState::mcp_sessions`]),
//!   the toolkits over the orchestration engine and projections, and the
//!   `previewAutomation.*` RPC methods the desktop's browser host uses.
//! - The registry itself is built with the core ([`super::App::build`]): it is the provider
//!   service's credential hook and, through `DriverEnv::mcp_sessions`, the drivers' session
//!   lookup. [`ClaudeMcpSessions`] and [`CodexMcpSessions`] adapt it to each driver's own
//!   lookup trait, so a driver plugs in with
//!   `options.mcp_sessions = env.mcp_sessions.clone().map(ClaudeMcpSessions::shared);`.

use std::sync::Arc;

use axum::Router;
use zc_mcp::{McpHttpOptions, McpServices, OrchestrationPullRequests, PreviewAutomationBroker, Toolkit};
use zc_providers::hooks::McpSessionReader;
use zc_rpc::RpcRouterBuilder;

use super::{AppState, Plugin};

/// `/mcp` and `previewAutomation.*`.
pub struct McpPlugin {
    broker: PreviewAutomationBroker,
    router: Router,
}

impl McpPlugin {
    pub fn new(state: &Arc<AppState>) -> Self {
        let broker = PreviewAutomationBroker::new();
        let paths = &state.config.paths;
        let toolkit = Toolkit::new(McpServices {
            broker: broker.clone(),
            pull_requests: Arc::new(OrchestrationPullRequests {
                reads: state.reads.clone(),
                engine: Arc::new(state.engine.clone()),
            }),
            attachments_dir: paths.attachments_dir.clone(),
            browser_artifacts_dir: paths.browser_artifacts_dir.clone(),
        });
        let router = zc_mcp::router(
            state.mcp_sessions.clone(),
            toolkit,
            McpHttpOptions {
                server_name: zc_mcp::BRAND_NAME.to_owned(),
                server_version: super::SERVER_VERSION.to_owned(),
                allowed_origins: Vec::new(),
            },
        );
        Self { broker, router }
    }

    /// The preview automation broker (the browser hosts' side of the preview tools).
    pub fn broker(&self) -> &PreviewAutomationBroker {
        &self.broker
    }
}

#[async_trait::async_trait]
impl Plugin for McpPlugin {
    fn name(&self) -> &'static str {
        "mcp"
    }

    fn register_rpc(&self, builder: RpcRouterBuilder) -> RpcRouterBuilder {
        zc_mcp::rpc::register(builder, self.broker.clone())
    }

    fn routes(&self) -> Router {
        self.router.clone()
    }
}

/// The registry as the Claude driver's `McpSessionLookup` (`--mcp-config` with the
/// `t3-code` server and its bearer header).
pub struct ClaudeMcpSessions(pub Arc<dyn McpSessionReader>);

impl ClaudeMcpSessions {
    pub fn shared(reader: Arc<dyn McpSessionReader>) -> Arc<dyn zc_provider_claude::adapter::McpSessionLookup> {
        Arc::new(Self(reader))
    }
}

impl zc_provider_claude::adapter::McpSessionLookup for ClaudeMcpSessions {
    fn read(&self, thread_id: &str) -> Option<zc_provider_claude::adapter::McpProviderSession> {
        self.0.read(thread_id).map(|session| zc_provider_claude::adapter::McpProviderSession {
            endpoint: session.endpoint,
            authorization_header: session.authorization_header,
            agent_device_environment: session.agent_device_environment,
        })
    }
}

/// The registry as the Codex driver's `McpSessionLookup` (`-c mcp_servers.t3-code.url=…`
/// with the token in `T3_MCP_BEARER_TOKEN`).
pub struct CodexMcpSessions(pub Arc<dyn McpSessionReader>);

impl CodexMcpSessions {
    pub fn shared(reader: Arc<dyn McpSessionReader>) -> Arc<dyn zc_provider_codex::adapter::McpSessionLookup> {
        Arc::new(Self(reader))
    }
}

impl zc_provider_codex::adapter::McpSessionLookup for CodexMcpSessions {
    fn read(&self, thread_id: &zc_contracts::ThreadId) -> Option<zc_provider_codex::adapter::McpProviderSession> {
        self.0.read(thread_id.as_str()).map(|session| zc_provider_codex::adapter::McpProviderSession {
            endpoint: session.endpoint,
            authorization_header: session.authorization_header,
            capabilities: session.capabilities.into_iter().collect(),
            agent_device_environment: session.agent_device_environment,
        })
    }
}

#[cfg(test)]
mod tests {
    use zc_mcp::{McpCapability, McpSessionRegistry, McpSessionRegistryOptions};

    use super::*;

    #[test]
    fn hands_the_threads_session_to_claude_and_codex() {
        let registry = McpSessionRegistry::new("environment-1", McpSessionRegistryOptions::default());
        registry.set_listen_address(Some("0.0.0.0"), 3773);
        let issued = registry.prepare_session("thread-1", "claudeAgent", &[McpCapability::PullRequests, McpCapability::Preview]);
        let reader: Arc<dyn McpSessionReader> = Arc::new(registry.clone());

        let claude = ClaudeMcpSessions::shared(reader.clone()).read("thread-1").unwrap();
        assert_eq!(claude.endpoint, "http://127.0.0.1:3773/mcp");
        assert_eq!(claude.authorization_header, issued.authorization_header);
        assert!(ClaudeMcpSessions::shared(reader.clone()).read("thread-2").is_none());

        let codex = CodexMcpSessions::shared(reader).read(&zc_contracts::ThreadId::new("thread-1")).unwrap();
        assert_eq!(codex.endpoint, "http://127.0.0.1:3773/mcp");
        assert_eq!(
            codex.capabilities.into_iter().collect::<Vec<_>>(),
            vec!["preview".to_owned(), "pull-requests".to_owned()]
        );
        // The credential the driver hands out is the one `/mcp` accepts.
        let token = issued.authorization_header.trim_start_matches("Bearer ").to_owned();
        assert_eq!(registry.resolve(&token).unwrap().thread_id, "thread-1");
    }
}
