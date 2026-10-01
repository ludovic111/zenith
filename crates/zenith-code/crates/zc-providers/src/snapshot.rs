//! `ServerProvider` snapshot building blocks shared by every driver: port of
//! `provider/providerSnapshot.ts`, `unavailableProviderSnapshot.ts`,
//! `Drivers/instanceIdentity.ts` and the version-advisory / manual-maintenance half of
//! `providerMaintenance.ts` (the update runners and installers are WP-12b).

use serde_json::Value;
use zc_contracts::{
    ModelCapabilities, ProviderDriverKind, ProviderInstanceId, ServerProvider, ServerProviderAuth, ServerProviderAuthStatus, ServerProviderAvailability,
    ServerProviderContinuation, ServerProviderModel, ServerProviderSkill, ServerProviderSlashCommand, ServerProviderState, ServerProviderUsageLimits,
    ServerProviderVersionAdvisory, ServerProviderVersionAdvisoryStatus,
};

use crate::semver::compare_semver_versions;

pub const DEFAULT_TIMEOUT_MS: u64 = 4_000;
/// Auth status checks can be slow on first run.
pub const AUTH_PROBE_TIMEOUT_MS: u64 = 10_000;
/// `PROVIDER_UPDATE_ACTION_TOAST_MESSAGE` (`providerMaintenance.ts`).
pub const PROVIDER_UPDATE_ACTION_TOAST_MESSAGE: &str = "Install the update now or review provider settings.";

/// `COMPACT_SLASH_COMMAND`.
pub fn compact_slash_command() -> ServerProviderSlashCommand {
    ServerProviderSlashCommand {
        name: "compact".into(),
        description: Some("Summarize the conversation and reduce context usage".into()),
        input: None,
    }
}

/// `ProviderProbeResult`: what a driver's status check found.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderProbeResult {
    pub installed: bool,
    pub version: Option<String>,
    /// Never `Disabled` (that is derived from `enabled`).
    pub status: ServerProviderState,
    pub auth: ServerProviderAuth,
    pub message: Option<String>,
    pub usage_limits: Option<ServerProviderUsageLimits>,
}

impl ProviderProbeResult {
    /// `auth: { status }` with nothing else known.
    pub fn auth(status: ServerProviderAuthStatus) -> ServerProviderAuth {
        ServerProviderAuth {
            status,
            r#type: None,
            label: None,
            email: None,
            subscription_sharing: None,
            profile_id: None,
        }
    }
}

/// `ServerProviderPresentation`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ServerProviderPresentation {
    pub display_name: String,
    pub badge_label: Option<String>,
    pub show_interaction_mode_toggle: Option<bool>,
    pub reports_context_window: Option<bool>,
    pub requires_new_thread_for_model_change: Option<bool>,
    pub supports_conversation_rollback: Option<bool>,
}

/// `buildServerProvider` input.
#[derive(Debug, Clone)]
pub struct BuildServerProviderInput {
    /// When set, the snapshot carries a version advisory for this driver.
    pub driver: Option<ProviderDriverKind>,
    pub presentation: ServerProviderPresentation,
    pub enabled: bool,
    pub checked_at: String,
    pub models: Vec<ServerProviderModel>,
    pub slash_commands: Vec<ServerProviderSlashCommand>,
    pub skills: Vec<ServerProviderSkill>,
    pub probe: ProviderProbeResult,
}

/// `ServerProviderDraft`: a snapshot before [`with_instance_identity`] stamps `instanceId`,
/// `driver`, the display overrides and the continuation group onto it. Its `instance_id` and
/// `driver` are empty until then.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerProviderDraft(pub ServerProvider);

/// `buildServerProvider(input)`.
pub fn build_server_provider(input: BuildServerProviderInput) -> ServerProviderDraft {
    let version_advisory = input.driver.as_ref().map(|driver| {
        create_provider_version_advisory(
            driver,
            input.probe.version.as_deref(),
            None,
            Some(&input.checked_at),
            &ProviderMaintenanceCapabilities::manual_only(driver.clone(), None),
        )
    });
    ServerProviderDraft(ServerProvider {
        instance_id: ProviderInstanceId::default(),
        driver: ProviderDriverKind::default(),
        display_name: Some(input.presentation.display_name),
        accent_color: None,
        badge_label: input.presentation.badge_label.filter(|label| !label.is_empty()),
        continuation: None,
        show_interaction_mode_toggle: input.presentation.show_interaction_mode_toggle,
        reports_context_window: input.presentation.reports_context_window,
        requires_new_thread_for_model_change: input.presentation.requires_new_thread_for_model_change,
        supports_conversation_rollback: input.presentation.supports_conversation_rollback,
        supports_text_generation: None,
        setup: None,
        runtime_paths: None,
        enabled: input.enabled,
        installed: input.probe.installed,
        version: input.probe.version,
        status: if input.enabled { input.probe.status } else { ServerProviderState::Disabled },
        auth: input.probe.auth,
        checked_at: input.checked_at,
        message: input.probe.message.filter(|message| !message.is_empty()),
        availability: None,
        unavailable_reason: None,
        models: input.models,
        slash_commands: input.slash_commands,
        skills: input.skills,
        workspace_snapshots: None,
        usage_limits: input.probe.usage_limits,
        version_advisory,
        compatibility_advisory: None,
        update_state: None,
    })
}

/// What `withInstanceIdentity` stamps.
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceIdentity {
    pub instance_id: ProviderInstanceId,
    pub driver_kind: ProviderDriverKind,
    pub display_name: Option<String>,
    pub accent_color: Option<String>,
    pub continuation_group_key: String,
}

/// `withInstanceIdentity(identity)(draft)`.
pub fn with_instance_identity(identity: &InstanceIdentity, draft: ServerProviderDraft) -> ServerProvider {
    let mut snapshot = draft.0;
    snapshot.instance_id = identity.instance_id.clone();
    snapshot.driver = identity.driver_kind.clone();
    if let Some(display_name) = identity.display_name.as_ref().filter(|name| !name.is_empty()) {
        snapshot.display_name = Some(display_name.clone());
    }
    if let Some(accent_color) = identity.accent_color.as_ref().filter(|color| !color.is_empty()) {
        snapshot.accent_color = Some(accent_color.clone());
    }
    snapshot.continuation = Some(ServerProviderContinuation {
        group_key: identity.continuation_group_key.clone(),
    });
    snapshot
}

/// `buildUnavailableProviderSnapshot`: a configured instance whose driver this build does not
/// ship, or whose config / creation failed.
pub fn build_unavailable_provider_snapshot(
    driver_kind: &ProviderDriverKind,
    instance_id: &ProviderInstanceId,
    display_name: Option<&str>,
    accent_color: Option<&str>,
    reason: &str,
    checked_at: Option<String>,
) -> ServerProvider {
    let checked_at = checked_at.unwrap_or_else(zc_core::now_iso);
    let display_name = display_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(driver_kind.as_str())
        .to_owned();
    let mut base = build_server_provider(BuildServerProviderInput {
        driver: None,
        presentation: ServerProviderPresentation {
            display_name,
            ..Default::default()
        },
        enabled: false,
        checked_at,
        models: Vec::new(),
        slash_commands: Vec::new(),
        skills: Vec::new(),
        probe: ProviderProbeResult {
            installed: false,
            version: None,
            status: ServerProviderState::Error,
            auth: ProviderProbeResult::auth(ServerProviderAuthStatus::Unknown),
            message: Some(reason.to_owned()),
            usage_limits: None,
        },
    })
    .0;
    base.instance_id = instance_id.clone();
    base.accent_color = accent_color.filter(|color| !color.is_empty()).map(str::to_owned);
    base.driver = driver_kind.clone();
    base.availability = Some(ServerProviderAvailability::Unavailable);
    base.unavailable_reason = Some(reason.to_owned());
    base
}

// ---------------------------------------------------------------------------------------------
// Maintenance capabilities (the read side; runners and resolvers are WP-12b)
// ---------------------------------------------------------------------------------------------

/// `ProviderMaintenanceCommandAction`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderMaintenanceCommandAction {
    pub command: String,
    pub executable: String,
    pub args: Vec<String>,
    pub lock_key: String,
    pub env: Option<Vec<(String, String)>>,
}

/// `ProviderMaintenanceCapabilities`: how (and whether) this instance's CLI can be updated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderMaintenanceCapabilities {
    pub provider: ProviderDriverKind,
    pub package_name: Option<String>,
    pub update: Option<ProviderMaintenanceCommandAction>,
    /// `None`: the installer has no channel of its own (npm is authoritative);
    /// `Some(None)`: the installer was asked and did not know.
    pub latest_version: Option<Option<String>>,
}

impl ProviderMaintenanceCapabilities {
    /// `makeManualOnlyProviderMaintenanceCapabilities`.
    pub fn manual_only(provider: ProviderDriverKind, package_name: Option<String>) -> Self {
        Self {
            provider,
            package_name,
            update: None,
            latest_version: None,
        }
    }
}

/// `makeTargetedProviderUpdateAction`: the update command pinned to `version`, when the installer
/// supports pinning.
pub fn make_targeted_provider_update_action(capabilities: &ProviderMaintenanceCapabilities, version: &str) -> Option<ProviderMaintenanceCommandAction> {
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() != 3 || !parts.iter().all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    let update = capabilities.update.as_ref()?;
    let package_name = capabilities.package_name.as_ref()?;
    let lock_key_ok = update.lock_key.starts_with("npm-global:") || ["bun-global", "pnpm-global", "vite-plus-global"].contains(&update.lock_key.as_str());
    if !lock_key_ok {
        return None;
    }
    let latest = format!("{package_name}@latest");
    let package_index = update.args.iter().position(|arg| *arg == latest || arg == package_name)?;
    let previous = update.args[package_index].clone();
    let pinned = format!("{package_name}@{version}");
    let args = update
        .args
        .iter()
        .enumerate()
        .map(|(index, arg)| if index == package_index { pinned.clone() } else { arg.clone() })
        .collect();
    let command = match update.command.rfind(&previous) {
        Some(index) => format!("{}{}{}", &update.command[..index], pinned, &update.command[index + previous.len()..]),
        None => update.command.clone(),
    };
    Some(ProviderMaintenanceCommandAction {
        command,
        args,
        ..update.clone()
    })
}

/// `createProviderVersionAdvisory`.
pub fn create_provider_version_advisory(
    _driver: &ProviderDriverKind,
    current_version: Option<&str>,
    latest_version: Option<&str>,
    checked_at: Option<&str>,
    capabilities: &ProviderMaintenanceCapabilities,
) -> ServerProviderVersionAdvisory {
    let (status, message) = match (current_version.filter(|v| !v.is_empty()), latest_version.filter(|v| !v.is_empty())) {
        (Some(current), Some(latest)) if compare_semver_versions(current, latest) == std::cmp::Ordering::Less => (
            ServerProviderVersionAdvisoryStatus::BehindLatest,
            Some(PROVIDER_UPDATE_ACTION_TOAST_MESSAGE.to_owned()),
        ),
        (Some(_), Some(_)) => (ServerProviderVersionAdvisoryStatus::Current, None),
        _ => (ServerProviderVersionAdvisoryStatus::Unknown, None),
    };
    ServerProviderVersionAdvisory {
        status,
        current_version: current_version.map(str::to_owned),
        latest_version: latest_version.map(str::to_owned),
        update_command: capabilities.update.as_ref().map(|update| update.command.clone()),
        can_update: capabilities.update.is_some(),
        can_install_version: Some(make_targeted_provider_update_action(capabilities, "0.0.0").is_some()),
        checked_at: checked_at.map(str::to_owned),
        message,
    }
}

// ---------------------------------------------------------------------------------------------
// Models and CLI output
// ---------------------------------------------------------------------------------------------

/// `nonEmptyTrimmed`.
pub fn non_empty_trimmed(value: Option<&str>) -> Option<String> {
    value.map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned)
}

/// `parseGenericCliVersion`: the first `x.y.z` in CLI output, with an optional leading `v`.
pub fn parse_generic_cli_version(output: &str) -> Option<String> {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pattern = PATTERN.get_or_init(|| regex::Regex::new(r"\bv?(\d+\.\d+\.\d+)\b").expect("valid regex"));
    pattern.captures(output).and_then(|captures| captures.get(1)).map(|m| m.as_str().to_owned())
}

/// One resolved `customModels` entry (`CustomModelDefinition`).
#[derive(Debug, Clone, PartialEq)]
pub struct CustomModelDefinition {
    pub slug: String,
    pub name: String,
    pub capabilities: Option<ModelCapabilities>,
}

/// shared `readCustomModelEntries`: bare slugs or `{slug, name?, capabilities?}`, trimmed,
/// deduplicated (first wins), malformed rows and capabilities dropped.
pub fn read_custom_model_entries(value: &Value) -> Vec<CustomModelDefinition> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    let mut entries: Vec<CustomModelDefinition> = Vec::new();
    for raw in items {
        let (slug, name, capabilities) = match raw {
            Value::String(slug) => (Some(slug.as_str()), None, None),
            Value::Object(record) => (
                record.get("slug").and_then(Value::as_str),
                record.get("name").and_then(Value::as_str),
                record.get("capabilities"),
            ),
            _ => continue,
        };
        let Some(slug) = slug.map(str::trim).filter(|slug| !slug.is_empty()) else {
            continue;
        };
        if entries.iter().any(|entry| entry.slug == slug) {
            continue;
        }
        let name = name.map(str::trim).filter(|name| !name.is_empty()).unwrap_or(slug).to_owned();
        let capabilities = match capabilities {
            None | Some(Value::Null) => None,
            Some(raw) => serde_json::from_value::<ModelCapabilities>(raw.clone())
                .ok()
                .map(|capabilities| ModelCapabilities {
                    option_descriptors: Some(capabilities.option_descriptors.unwrap_or_default()),
                }),
        };
        entries.push(CustomModelDefinition {
            slug: slug.to_owned(),
            name,
            capabilities,
        });
    }
    entries
}

/// `providerModelsFromSettings`: built-ins, then the user's custom models (a slug colliding with
/// a built-in is dropped; a bare slug gets `custom_model_capabilities`).
pub fn provider_models_from_settings(
    built_in: &[ServerProviderModel],
    custom_models: &Value,
    custom_model_capabilities: &ModelCapabilities,
) -> Vec<ServerProviderModel> {
    let mut models = built_in.to_vec();
    for entry in read_custom_model_entries(custom_models) {
        if models.iter().any(|model| model.slug == entry.slug) {
            continue;
        }
        models.push(ServerProviderModel {
            slug: entry.slug,
            name: entry.name,
            short_name: None,
            sub_provider: None,
            aliases: None,
            badge: None,
            is_custom: true,
            is_default: None,
            is_legacy: None,
            capabilities: Some(entry.capabilities.unwrap_or_else(|| custom_model_capabilities.clone())),
        });
    }
    models
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn capabilities(value: Value) -> ModelCapabilities {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn custom_models_follow_settings() {
        let default = capabilities(json!({"optionDescriptors": [{"id": "variant", "label": "Variant", "type": "select", "options": []}]}));
        let own = capabilities(json!({"optionDescriptors": [{"id": "fastMode", "label": "Fast Mode", "type": "boolean"}]}));
        let models = provider_models_from_settings(
            &[],
            &json!(["bare", {"slug": "named", "name": "Named", "capabilities": own}, " bare "]),
            &default,
        );
        assert_eq!(
            serde_json::to_value(&models).unwrap(),
            json!([
                {"slug": "bare", "name": "bare", "isCustom": true, "capabilities": default},
                {"slug": "named", "name": "Named", "isCustom": true, "capabilities": own}
            ])
        );
    }

    #[test]
    fn parses_generic_cli_versions() {
        assert_eq!(parse_generic_cli_version("1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(parse_generic_cli_version("opencode v2.0.3").as_deref(), Some("2.0.3"));
        assert_eq!(parse_generic_cli_version("tool version 10.20.30 (build)").as_deref(), Some("10.20.30"));
        assert_eq!(parse_generic_cli_version("no version here"), None);
        assert_eq!(parse_generic_cli_version("x1.2.3y"), None);
    }

    #[test]
    fn unavailable_snapshots_match_the_wire_shape() {
        let snapshot = build_unavailable_provider_snapshot(
            &ProviderDriverKind::from("forkDriver"),
            &ProviderInstanceId::from("fork_one"),
            None,
            Some("#ff0000"),
            "Driver 'forkDriver' is not registered in this build.",
            Some("2026-01-01T00:00:00.000Z".into()),
        );
        assert_eq!(
            serde_json::to_value(&snapshot).unwrap(),
            json!({
                "instanceId": "fork_one",
                "driver": "forkDriver",
                "displayName": "forkDriver",
                "accentColor": "#ff0000",
                "enabled": false,
                "installed": false,
                "version": null,
                "status": "disabled",
                "auth": {"status": "unknown"},
                "checkedAt": "2026-01-01T00:00:00.000Z",
                "message": "Driver 'forkDriver' is not registered in this build.",
                "availability": "unavailable",
                "unavailableReason": "Driver 'forkDriver' is not registered in this build.",
                "models": [],
                "slashCommands": [],
                "skills": []
            })
        );
    }

    #[test]
    fn version_advisories() {
        let manual = ProviderMaintenanceCapabilities::manual_only("codex".into(), None);
        let advisory = create_provider_version_advisory(&"codex".into(), Some("1.0.0"), Some("1.1.0"), Some("t"), &manual);
        assert_eq!(advisory.status, ServerProviderVersionAdvisoryStatus::BehindLatest);
        assert!(!advisory.can_update);
        assert_eq!(advisory.can_install_version, Some(false));
        let npm = ProviderMaintenanceCapabilities {
            provider: "codex".into(),
            package_name: Some("@openai/codex".into()),
            update: Some(ProviderMaintenanceCommandAction {
                command: "npm install -g @openai/codex@latest".into(),
                executable: "npm".into(),
                args: vec!["install".into(), "-g".into(), "@openai/codex@latest".into()],
                lock_key: "npm-global:/usr/local".into(),
                env: None,
            }),
            latest_version: None,
        };
        let pinned = make_targeted_provider_update_action(&npm, "1.2.3").unwrap();
        assert_eq!(pinned.command, "npm install -g @openai/codex@1.2.3");
        assert_eq!(pinned.args[2], "@openai/codex@1.2.3");
    }
}
