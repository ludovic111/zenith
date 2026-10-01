//! `McpProviderSession.ts`: the credentials a provider session hands its agent, and the
//! environment the `agent-device` CLI needs.

use std::collections::{BTreeMap, HashMap};

/// `McpProviderSessionConfig`: what a driver needs to give its agent the `t3-code` MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpProviderSessionConfig {
    pub environment_id: String,
    pub thread_id: String,
    pub provider_session_id: String,
    pub provider_instance_id: String,
    /// `http://<host>:<port>/mcp`.
    pub endpoint: String,
    /// `Bearer <token>`.
    pub authorization_header: String,
    /// The capabilities the credential grants (`pull-requests`, `preview`, `device`), sorted.
    pub capabilities: Vec<String>,
    /// Set when the session may drive devices: spread into the provider subprocess environment
    /// so the `agent-device` CLI is on `PATH` (`PATH` is prepended, `PATH_SEPARATOR` names the
    /// separator).
    pub agent_device_environment: Option<BTreeMap<String, String>>,
}

/// `withAgentDeviceEnvironment`: the provider environment with the device variables applied
/// over `base`, or `base` untouched.
pub fn with_agent_device_environment(base: &HashMap<String, String>, config: Option<&McpProviderSessionConfig>) -> HashMap<String, String> {
    let Some(extra) = config.and_then(|config| config.agent_device_environment.as_ref()) else {
        return base.clone();
    };
    if extra.is_empty() {
        return base.clone();
    }
    let separator = extra.get("PATH_SEPARATOR").map(String::as_str).unwrap_or(":");
    let base_path = base.get("PATH").or_else(|| base.get("Path"));
    let mut environment = base.clone();
    for (key, value) in extra {
        if key != "PATH" && key != "PATH_SEPARATOR" {
            environment.insert(key.clone(), value.clone());
        }
    }
    if let Some(shim_dir) = extra.get("PATH").filter(|dir| !dir.is_empty()) {
        let path = match base_path.filter(|path| !path.is_empty()) {
            Some(base_path) => format!("{shim_dir}{separator}{base_path}"),
            None => shim_dir.clone(),
        };
        environment.insert("PATH".into(), path);
    }
    environment
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(extra: Option<BTreeMap<String, String>>) -> McpProviderSessionConfig {
        McpProviderSessionConfig {
            environment_id: "environment-1".into(),
            thread_id: "thread-1".into(),
            provider_session_id: "session-1".into(),
            provider_instance_id: "codex".into(),
            endpoint: "http://127.0.0.1:1/mcp".into(),
            authorization_header: "Bearer x".into(),
            capabilities: vec!["pull-requests".into()],
            agent_device_environment: extra,
        }
    }

    // McpProviderSession.test.ts: "preserves provider credentials and commands while routing
    // devices to the owned daemon"
    #[test]
    fn preserves_provider_credentials_and_commands_while_routing_devices_to_the_owned_daemon() {
        let base = HashMap::from([
            ("PATH".to_string(), "/provider/bin:/usr/bin".to_string()),
            ("PROVIDER_KEY".to_string(), "fixture".to_string()),
        ]);
        let extra = BTreeMap::from([
            ("PATH".to_string(), "/t3/device/bin".to_string()),
            ("PATH_SEPARATOR".to_string(), ":".to_string()),
            ("AGENT_DEVICE_DAEMON_BASE_URL".to_string(), "http://127.0.0.1:9000".to_string()),
            ("AGENT_DEVICE_DAEMON_AUTH_TOKEN".to_string(), "fixture-device".to_string()),
        ]);
        let environment = with_agent_device_environment(&base, Some(&config(Some(extra))));
        assert_eq!(
            environment,
            HashMap::from([
                ("PATH".to_string(), "/t3/device/bin:/provider/bin:/usr/bin".to_string()),
                ("PROVIDER_KEY".to_string(), "fixture".to_string()),
                ("AGENT_DEVICE_DAEMON_BASE_URL".to_string(), "http://127.0.0.1:9000".to_string()),
                ("AGENT_DEVICE_DAEMON_AUTH_TOKEN".to_string(), "fixture-device".to_string()),
            ])
        );
    }

    // "does not grant CLI access when device access was not supplied"
    #[test]
    fn does_not_grant_cli_access_when_device_access_was_not_supplied() {
        let base = HashMap::from([
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("PROVIDER_KEY".to_string(), "fixture".to_string()),
        ]);
        assert_eq!(with_agent_device_environment(&base, None), base);
        assert_eq!(with_agent_device_environment(&base, Some(&config(None))), base);
        assert_eq!(with_agent_device_environment(&base, Some(&config(Some(BTreeMap::new())))), base);
    }
}
