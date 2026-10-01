//! `resolveProjectSettings(settings, projectId).settings` (`@t3tools/shared/projectSettings`)
//! on encoded settings, the way the reactors call it (no legacy project fields, no t3.json).

use serde_json::Value;
use zc_ports::contracts::ServerSettings;

/// `PROJECT_SCOPED_SERVER_SETTING_KEYS`.
pub const PROJECT_SCOPED_SERVER_SETTING_KEYS: &[&str] = &[
    "worktreeCleanup",
    "defaultModelSelection",
    "defaultRuntimeMode",
    "defaultThreadEnvMode",
    "newWorktreesStartFromOrigin",
    "worktreeSubmodules",
    "defaultAutoPull",
    "defaultProjectScripts",
    "enableAgentBrowserAccess",
    "enableAgentDeviceAccess",
    "textGenerationModelSelection",
    "sourceControlWriterModelSelection",
    "sourceControlWritingStyle",
    "pullRequestMergeMethod",
    "sidebarAutoSettleOnMerge",
    "sidebarAutoSettleAfterDays",
    "continueThreadsAfterServerUpdate",
    "responseStreamingMode",
];

/// The wire JSON of the settings.
pub fn settings_json(settings: &ServerSettings) -> Value {
    serde_json::to_value(settings).unwrap_or(Value::Null)
}

/// `resolveProjectSettings(settings, projectId).settings`: the environment settings with the
/// project's overrides applied (a model override on a disabled provider falls back).
pub fn resolve_project_settings(settings: &Value, project_id: Option<&str>) -> Value {
    let Some(overrides) = project_id
        .and_then(|id| settings["projectSettingsOverrides"].get(id))
        .and_then(Value::as_object)
    else {
        return settings.clone();
    };
    if overrides.is_empty() {
        return settings.clone();
    }
    let mut effective = settings.clone();
    for key in PROJECT_SCOPED_SERVER_SETTING_KEYS {
        let Some(value) = overrides.get(*key) else { continue };
        if (*key == "textGenerationModelSelection" || *key == "defaultModelSelection")
            && !value.is_null()
            && !zc_settings::settings::logic::is_model_selection_provider_enabled(settings, value)
        {
            continue;
        }
        effective[*key] = value.clone();
    }
    effective
}

/// `hasProjectSettingsOverrides`-free check the command reactor does first:
/// `Object.keys(settings.projectSettingsOverrides).length === 0`.
pub fn has_any_project_overrides(settings: &Value) -> bool {
    settings["projectSettingsOverrides"].as_object().is_some_and(|entries| !entries.is_empty())
}

/// `responseStreamingMode` (`"turn" | "paragraph" | "token"`, default paragraph).
pub fn response_streaming_mode(settings: &Value) -> String {
    settings["responseStreamingMode"].as_str().unwrap_or("paragraph").to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn applies_project_overrides() {
        let settings = json!({
            "responseStreamingMode": "paragraph",
            "projectSettingsOverrides": {"p1": {"responseStreamingMode": "token"}},
            "providerInstances": {},
            "providers": {},
        });
        assert_eq!(response_streaming_mode(&resolve_project_settings(&settings, Some("p1"))), "token");
        assert_eq!(response_streaming_mode(&resolve_project_settings(&settings, Some("p2"))), "paragraph");
        assert_eq!(response_streaming_mode(&resolve_project_settings(&settings, None)), "paragraph");
    }
}
