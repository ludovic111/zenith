//! `McpInvocationContext.ts`: who is calling (the credential's scope) and the capability check
//! every tool starts with.

use std::collections::HashSet;

use zc_ports::TaggedError;

pub use zc_providers::hooks::McpCapability;

/// `McpInvocationScope`: what a resolved bearer credential stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpInvocationScope {
    pub environment_id: String,
    pub thread_id: String,
    pub provider_session_id: String,
    pub provider_instance_id: String,
    pub capabilities: HashSet<McpCapability>,
    pub issued_at: i64,
}

impl McpInvocationScope {
    pub fn has(&self, capability: McpCapability) -> bool {
        self.capabilities.contains(&capability)
    }

    /// The capabilities as strings, sorted.
    pub fn capability_names(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = self.capabilities.iter().map(|c| c.as_str()).collect();
        names.sort_unstable();
        names
    }
}

/// `McpCapability` from its wire name.
pub fn capability_from_str(name: &str) -> Option<McpCapability> {
    match name {
        "pull-requests" => Some(McpCapability::PullRequests),
        "preview" => Some(McpCapability::Preview),
        "device" => Some(McpCapability::Device),
        _ => None,
    }
}

/// `requireMcpCapability`: the scope when it grants `capability`, otherwise
/// `PreviewAutomationUnavailableError` (preview, so the broker's callers can route it) or
/// `McpCapabilityUnavailableError`.
pub fn require_capability(scope: &McpInvocationScope, capability: McpCapability) -> Result<&McpInvocationScope, TaggedError> {
    if scope.has(capability) {
        return Ok(scope);
    }
    Err(crate::errors::capability_unavailable(scope, capability))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(capabilities: &[McpCapability]) -> McpInvocationScope {
        McpInvocationScope {
            environment_id: "environment-1".into(),
            thread_id: "thread-1".into(),
            provider_session_id: "provider-session-1".into(),
            provider_instance_id: "codex".into(),
            capabilities: capabilities.iter().copied().collect(),
            issued_at: 1,
        }
    }

    // McpInvocationContext.test.ts: "reports the scoped credential context when preview
    // capability is unavailable"
    #[test]
    fn reports_the_scoped_credential_context_when_preview_capability_is_unavailable() {
        let invocation = scope(&[]);
        let error = require_capability(&invocation, McpCapability::Preview).unwrap_err();
        assert_eq!(error.tag, "PreviewAutomationUnavailableError");
        assert_eq!(error.fields["capability"], "preview");
        assert_eq!(error.fields["environmentId"], "environment-1");
        assert_eq!(error.fields["threadId"], "thread-1");
        assert_eq!(error.fields["providerSessionId"], "provider-session-1");
        assert_eq!(error.fields["providerInstanceId"], "codex");
        assert!(error.message.contains("MCP credential does not grant the preview capability"));
        assert!(error.message.contains("use a headless browser from the shell"));
    }

    // "reports other missing capabilities with the neutral error"
    #[test]
    fn reports_other_missing_capabilities_with_the_neutral_error() {
        let invocation = scope(&[McpCapability::Preview]);
        let error = require_capability(&invocation, McpCapability::PullRequests).unwrap_err();
        assert_eq!(error.tag, "McpCapabilityUnavailableError");
        assert_eq!(error.fields["capability"], "pull-requests");
        assert_eq!(error.fields["threadId"], "thread-1");
        assert_eq!(error.message, "MCP credential does not grant the pull-requests capability.");
        let granted = require_capability(&invocation, McpCapability::Preview).unwrap();
        assert_eq!(granted, &invocation);
    }
}
