//! `resolveProviderInstanceTerminalEnvironment` (`terminal/Manager.ts`): the environment of a
//! terminal opened for a provider instance (the "open a terminal with this agent's home"
//! action): the instance's environment variables over the client's, plus the instance's
//! `CODEX_HOME` (Codex) or `CLAUDE_CONFIG_DIR` (Claude).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use zc_contracts::{ClaudeSettings, CodexSettings, ProviderDriverKind};
use zc_core::Defect;
use zc_ports::SettingsService;
use zc_terminal::contracts::TerminalError;
use zc_terminal::ProviderEnvironmentResolver;

/// The terminal manager's provider environment, read from the server settings.
pub struct ProviderTerminalEnvironment {
    settings: Arc<dyn SettingsService>,
}

impl ProviderTerminalEnvironment {
    pub fn new(settings: Arc<dyn SettingsService>) -> Self {
        Self { settings }
    }
}

#[async_trait]
impl ProviderEnvironmentResolver for ProviderTerminalEnvironment {
    async fn resolve(&self, provider_instance_id: &str, env: Option<&BTreeMap<String, String>>) -> Result<BTreeMap<String, String>, TerminalError> {
        let settings = self
            .settings
            .get_settings()
            .await
            .map_err(|error| TerminalError::TerminalProviderEnvironmentError {
                provider_instance_id: provider_instance_id.to_owned(),
                cause: Defect::error("ServerSettingsError", format!("{error:?}")),
            })?;
        let settings = serde_json::to_value(&settings).unwrap_or_default();
        let kinds: Vec<ProviderDriverKind> = zc_providers::BUILT_IN_DRIVER_KINDS.iter().map(|kind| ProviderDriverKind::from(*kind)).collect();
        let Some((_, instance)) = zc_providers::settings::derive_provider_instance_config_map(&settings, &kinds)
            .into_iter()
            .find(|(id, _)| id.as_str() == provider_instance_id)
        else {
            return Err(TerminalError::TerminalProviderInstanceNotFoundError {
                provider_instance_id: provider_instance_id.to_owned(),
            });
        };
        Ok(resolve_instance_environment(&instance, env))
    }
}

/// The resolution itself, once the instance's settings entry is known.
pub fn resolve_instance_environment(instance: &zc_contracts::ProviderInstanceConfig, env: Option<&BTreeMap<String, String>>) -> BTreeMap<String, String> {
    let base: HashMap<String, String> = env.map(|env| env.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default();
    let variables = instance.environment.clone().unwrap_or_default();
    let mut resolved: BTreeMap<String, String> = zc_providers::driver::merge_provider_instance_environment(&variables, &base)
        .into_iter()
        .collect();
    let config = instance.config.clone().unwrap_or_else(|| serde_json::json!({}));
    match instance.driver.as_str() {
        "codex" => {
            if let Ok(config) = serde_json::from_value::<CodexSettings>(config) {
                let layout = zc_provider_codex::home_layout::resolve_codex_home_layout(&config);
                if let Some(home) = layout.effective_home_path {
                    resolved.insert("CODEX_HOME".into(), home.to_string_lossy().into_owned());
                }
            }
        }
        "claudeAgent" => {
            if let Ok(config) = serde_json::from_value::<ClaudeSettings>(config) {
                resolved = zc_provider_claude::home::make_claude_environment(&config.home_path, resolved);
            }
        }
        _ => {}
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn instance(value: serde_json::Value) -> zc_contracts::ProviderInstanceConfig {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn codex_instances_get_their_home() {
        let entry = instance(json!({
            "driver": "codex",
            "environment": [{"name": "SAMPLE_FLAG", "value": "on"}],
            "config": {"homePath": "/tmp/sample-codex-home"},
        }));
        let client = BTreeMap::from([("TERM_PROGRAM".to_owned(), "sample".to_owned())]);
        let env = resolve_instance_environment(&entry, Some(&client));
        assert_eq!(env["SAMPLE_FLAG"], "on");
        assert_eq!(env["TERM_PROGRAM"], "sample");
        assert_eq!(env["CODEX_HOME"], "/tmp/sample-codex-home");
    }

    #[test]
    fn claude_instances_get_their_config_dir_only_when_set() {
        let entry = instance(json!({"driver": "claudeAgent", "config": {"homePath": "/tmp/sample-claude-home"}}));
        assert_eq!(resolve_instance_environment(&entry, None)["CLAUDE_CONFIG_DIR"], "/tmp/sample-claude-home");
        let entry = instance(json!({"driver": "claudeAgent"}));
        assert!(!resolve_instance_environment(&entry, None).contains_key("CLAUDE_CONFIG_DIR"));
    }
}
