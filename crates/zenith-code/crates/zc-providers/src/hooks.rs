//! What the provider service and the reaper need from subsystems owned by other work packages,
//! as narrow traits with no-op defaults: MCP credentials (WP-27a), analytics (stubbed telemetry,
//! plan §6.19) and the projection's thread shells (WP-09, through `zc_ports::ProjectionReads`).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use zc_contracts::{ProviderInstanceId, ThreadId};
use zc_ports::ProjectionReads;

/// `McpInvocationContext.McpCapability`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum McpCapability {
    PullRequests,
    Preview,
    Device,
}

impl McpCapability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PullRequests => "pull-requests",
            Self::Preview => "preview",
            Self::Device => "device",
        }
    }
}

/// `McpSessionRegistry` + `McpProviderSession` as the provider service uses them.
#[async_trait]
pub trait McpSessions: Send + Sync {
    /// `issueActiveMcpCredential({threadId, providerInstanceId, capabilities})` followed by
    /// `setMcpProviderSession(...)` (with the agent-device environment when `Device` is granted).
    /// Returns whether a credential was issued (false when no MCP server is running).
    async fn prepare(&self, thread_id: &ThreadId, instance_id: &ProviderInstanceId, capabilities: &[McpCapability]) -> bool;
    /// `touchActiveMcpThread`: a turn keeps the session's credential alive.
    async fn touch(&self, thread_id: &ThreadId);
    /// `revokeActiveMcpThread` + `clearMcpProviderSession`.
    async fn clear(&self, thread_id: &ThreadId);
    /// `revokeAllActiveMcpCredentials` + `clearAllMcpProviderSessions` (shutdown).
    async fn revoke_all(&self);
}

/// `McpProviderSessionConfig` as a driver reads it: what to hand the agent so it gets the
/// `t3-code` MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSessionConfig {
    /// `http://<host>:<port>/mcp`.
    pub endpoint: String,
    /// `Bearer <token>`.
    pub authorization_header: String,
    /// `pull-requests`, `preview`, `device`, sorted.
    pub capabilities: Vec<String>,
    /// Variables that put the `agent-device` CLI on `PATH` (`PATH` is prepended).
    pub agent_device_environment: Option<std::collections::BTreeMap<String, String>>,
}

/// `McpProviderSession.readMcpProviderSession`: the session [`McpSessions::prepare`] recorded
/// for a thread. Drivers read it when they start a session (zc-mcp's registry implements it).
pub trait McpSessionReader: Send + Sync {
    fn read(&self, thread_id: &str) -> Option<McpSessionConfig>;
}

/// `AnalyticsService.record` / `flush` (PostHog, off by default in zenith).
#[async_trait]
pub trait ProviderAnalytics: Send + Sync {
    fn record(&self, event: &str, properties: Value);
    async fn flush(&self) {}
}

/// The fields of `OrchestrationThreadShell` the provider core reads.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ThreadShellInfo {
    pub project_id: Option<String>,
    /// `session.updatedAt`.
    pub session_updated_at: Option<String>,
    /// `session.activeTurnId`.
    pub session_active_turn_id: Option<String>,
    /// `backgroundLiveness` (non-null while sub-agents, workflows or monitors still run).
    pub background_liveness: Option<Value>,
}

/// `ProjectionSnapshotQuery.getThreadShellById`, narrowed.
#[async_trait]
pub trait ThreadShells: Send + Sync {
    async fn get_thread_shell(&self, thread_id: &ThreadId) -> Result<Option<ThreadShellInfo>, String>;
}

/// [`ThreadShells`] over the orchestration's projection reads.
pub struct ProjectionThreadShells(pub Arc<dyn ProjectionReads>);

fn non_null_string(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
}

#[async_trait]
impl ThreadShells for ProjectionThreadShells {
    async fn get_thread_shell(&self, thread_id: &ThreadId) -> Result<Option<ThreadShellInfo>, String> {
        let id = zc_ports::contracts::ThreadId::new(thread_id.as_str());
        let shell = self.0.get_thread_shell_by_id(&id).await.map_err(|error| error.to_string())?;
        Ok(shell.map(|shell| {
            let value = serde_json::to_value(&shell).unwrap_or_default();
            let session = value.get("session");
            ThreadShellInfo {
                project_id: non_null_string(value.get("projectId")),
                session_updated_at: non_null_string(session.and_then(|session| session.get("updatedAt"))),
                session_active_turn_id: non_null_string(session.and_then(|session| session.get("activeTurnId"))),
                background_liveness: value.get("backgroundLiveness").filter(|liveness| !liveness.is_null()).cloned(),
            }
        }))
    }
}
