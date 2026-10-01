//! Port of `packages/shared/src/serverSettings.test.ts` (the pure helpers).

use serde_json::{json, Value};
use zc_settings::settings::background;
use zc_settings::settings::logic::{
    apply_server_settings_patch, create_model_selection, is_model_selection_provider_enabled, parse_persisted_server_observability_settings,
    resolve_project_agent_browser_access, resolve_project_auto_pull, resolve_source_control_writer_model_selection, PersistedObservabilitySettings,
};
use zc_settings::settings::schema::{decode_patch, default_settings};

/// `applyServerSettingsPatch(current, patch)` with the patch decoded first, as the RPC does.
fn apply(current: &Value, patch: Value) -> Value {
    apply_server_settings_patch(current, &decode_patch(&patch).expect("patch decodes"))
}

fn folded() -> Value {
    let mut settings = default_settings();
    settings["projectSettingsFolded"] = json!(true);
    settings
}

fn with(mut settings: Value, key: &str, value: Value) -> Value {
    settings[key] = value;
    settings
}

fn selection(instance_id: &str, model: &str, options: Option<Value>) -> Value {
    create_model_selection(&json!(instance_id), &json!(model), options.as_ref())
}

fn action(command: &str) -> Value {
    json!({"id": "check", "name": "Check", "command": command, "icon": "play", "runOnWorktreeCreate": false})
}

#[test]
fn changes_a_cleanup_rule_without_replacing_the_machines_other_rules() {
    let enabled = apply(
        &default_settings(),
        json!({"storageCleanup": {"worktreeAfterDays": 8, "worktreeOnMerge": true, "logsAfterDays": 30}}),
    );
    let next = apply(&enabled, json!({"storageCleanup": {"worktreeAfterDays": null}}));
    assert_eq!(
        next["storageCleanup"],
        json!({
            "worktreeAfterDays": null,
            "worktreeOnMerge": true,
            "worktreeOnDelete": false,
            "worktreeUnchanged": false,
            "browserArtifactsAfterDays": null,
            "logsAfterDays": 30
        })
    );
}

#[test]
fn replaces_ssh_host_lists_when_saving_editing_and_removing_hosts() {
    let host = json!({"id": "mini", "label": "Mac mini", "target": "mini"});
    let saved = apply(&default_settings(), json!({"deviceHosts": [host]}));
    assert_eq!(saved["deviceHosts"], json!([host]));
    let replacement = json!({"id": "mini", "label": "Mac mini", "target": "other-mini"});
    let edited = apply(&saved, json!({"deviceHosts": [replacement]}));
    assert_eq!(edited["deviceHosts"], json!([replacement]));
    assert_eq!(apply(&edited, json!({"deviceHosts": []}))["deviceHosts"], json!([]));
}

#[test]
fn project_script_overrides_are_set_cleared_and_reset_per_project() {
    // The `resolveProjectScripts` half of the TS tests lives with the project scripts; here the
    // settings side: overrides land in `projectSettingsOverrides` and the derived map.
    let defaults = apply(&folded(), json!({"defaultProjectScripts": [action("npm test")]}));
    assert_eq!(defaults["defaultProjectScripts"], json!([action("npm test")]));
    let disabled = apply(&defaults, json!({"projectScriptOverrides": {"project-actions": []}}));
    assert_eq!(disabled["projectSettingsOverrides"]["project-actions"], json!({"defaultProjectScripts": []}));
    assert_eq!(disabled["projectScriptOverrides"], json!({"project-actions": []}));
    let changed = apply(&disabled, json!({"defaultProjectScripts": [action("npm run build")]}));
    assert_eq!(changed["projectScriptOverrides"], json!({"project-actions": []}));
    let reset = apply(&changed, json!({"projectScriptOverrides": {"project-actions": null}}));
    assert_eq!(reset["projectSettingsOverrides"], json!({}));
    assert_eq!(reset["projectScriptOverrides"], json!({}));
}

#[test]
fn preserves_other_projects_actions_when_overriding_clearing_or_resetting_one_project() {
    let first = apply(
        &folded(),
        json!({
            "defaultProjectScripts": [action("npm test")],
            "projectScriptOverrides": {"first-project": [action("npm run lint")]}
        }),
    );
    let second = apply(&first, json!({"projectScriptOverrides": {"second-project": [action("npm run build")]}}));
    assert_eq!(second["projectScriptOverrides"]["first-project"], json!([action("npm run lint")]));
    assert_eq!(second["projectScriptOverrides"]["second-project"], json!([action("npm run build")]));
    let cleared = apply(&second, json!({"projectScriptOverrides": {"first-project": []}}));
    assert_eq!(cleared["projectScriptOverrides"]["first-project"], json!([]));
    assert_eq!(cleared["projectScriptOverrides"]["second-project"], json!([action("npm run build")]));
    let reset = apply(&cleared, json!({"projectScriptOverrides": {"first-project": null}}));
    assert!(reset["projectScriptOverrides"].get("first-project").is_none());
    assert_eq!(reset["projectScriptOverrides"]["second-project"], json!([action("npm run build")]));
}

#[test]
fn inherits_automatic_pull_while_preserving_legacy_opt_ins_and_explicit_overrides() {
    let project = "project-pull";
    let defaults = default_settings();
    assert!(!resolve_project_auto_pull(&defaults, project, Some(false)));
    assert!(resolve_project_auto_pull(&defaults, project, Some(true)));
    let enabled = apply(&defaults, json!({"defaultAutoPull": true}));
    assert!(resolve_project_auto_pull(&enabled, project, Some(false)));
    let overridden = apply(&enabled, json!({"projectAutoPullOverrides": {project: false}}));
    assert!(!resolve_project_auto_pull(&overridden, project, Some(true)));
    let reset = apply(&overridden, json!({"projectAutoPullOverrides": {project: null}}));
    assert!(resolve_project_auto_pull(&reset, project, Some(false)));
    let disabled = apply(&reset, json!({"defaultAutoPull": false, "projectAutoPullOverrides": {project: true}}));
    assert!(resolve_project_auto_pull(&disabled, project, Some(false)));
    assert!(!resolve_project_auto_pull(&disabled, "other-project", Some(false)));
}

#[test]
fn inherits_browser_access_and_restores_inheritance_when_a_project_override_is_removed() {
    let overridden = apply(&default_settings(), json!({"projectAgentBrowserAccessOverrides": {"project-browser": false}}));
    assert!(!resolve_project_agent_browser_access(&overridden, "project-browser"));
    assert!(resolve_project_agent_browser_access(&overridden, "other-project"));
    let reset = apply(&overridden, json!({"projectAgentBrowserAccessOverrides": {"project-browser": null}}));
    assert!(resolve_project_agent_browser_access(&reset, "project-browser"));
    let enabled = apply(
        &reset,
        json!({"enableAgentBrowserAccess": false, "projectAgentBrowserAccessOverrides": {"project-browser": true}}),
    );
    assert!(resolve_project_agent_browser_access(&enabled, "project-browser"));
    assert!(!resolve_project_agent_browser_access(&enabled, "other-project"));
}

#[test]
fn preserves_other_projects_boolean_overrides_across_separate_updates_and_resets() {
    let first = apply(
        &default_settings(),
        json!({
            "defaultAutoPull": true,
            "projectAutoPullOverrides": {"first-project": false},
            "projectAgentBrowserAccessOverrides": {"first-project": false}
        }),
    );
    let second = apply(
        &first,
        json!({
            "projectAutoPullOverrides": {"second-project": false},
            "projectAgentBrowserAccessOverrides": {"second-project": false}
        }),
    );
    for project in ["first-project", "second-project"] {
        assert!(!resolve_project_auto_pull(&second, project, Some(false)));
        assert!(!resolve_project_agent_browser_access(&second, project));
    }
    let reset = apply(
        &second,
        json!({
            "projectAutoPullOverrides": {"first-project": null},
            "projectAgentBrowserAccessOverrides": {"first-project": null}
        }),
    );
    assert!(resolve_project_auto_pull(&reset, "first-project", Some(false)));
    assert!(resolve_project_agent_browser_access(&reset, "first-project"));
    assert!(!resolve_project_auto_pull(&reset, "second-project", Some(false)));
    assert!(!resolve_project_agent_browser_access(&reset, "second-project"));
    assert!(reset["projectAutoPullOverrides"].get("first-project").is_none());
    assert!(reset["projectAgentBrowserAccessOverrides"].get("first-project").is_none());
    assert!(!resolve_project_auto_pull(&second, "first-project", Some(false)));
}

#[test]
fn replaces_and_clears_conversation_model_defaults_without_retaining_old_options() {
    let current = apply(
        &default_settings(),
        json!({"defaultModelSelection": selection("codex", "gpt-5.4", Some(json!([{"id": "reasoningEffort", "value": "high"}])))}),
    );
    let sonnet = selection("claudeAgent", "sonnet", None);
    let updated = apply(&current, json!({"defaultModelSelection": sonnet}));
    assert_eq!(updated["defaultModelSelection"], sonnet);
    assert_eq!(apply(&updated, json!({"defaultModelSelection": null}))["defaultModelSelection"], Value::Null);
}

#[test]
fn ignores_missing_and_blank_persisted_observability_urls() {
    assert_eq!(parse_persisted_server_observability_settings("{}"), PersistedObservabilitySettings::default());
    assert_eq!(
        parse_persisted_server_observability_settings(r#"{"observability":{"otlpTracesUrl":"   ","otlpMetricsUrl":"","otlpLogsUrl":"   "}}"#),
        PersistedObservabilitySettings::default()
    );
}

#[test]
fn parses_lenient_persisted_settings_json_and_trims_observability_urls() {
    let parsed = parse_persisted_server_observability_settings(
        r#"{
          // comment
          "observability": {
            "otlpTracesUrl": "  http://localhost:4318/v1/traces  ",
            "otlpMetricsUrl": "  http://localhost:4318/v1/metrics  ",
            "otlpLogsUrl": "  http://localhost:4318/v1/logs  ",
          },
        }"#,
    );
    assert_eq!(
        parsed,
        PersistedObservabilitySettings {
            otlp_traces_url: Some("http://localhost:4318/v1/traces".into()),
            otlp_metrics_url: Some("http://localhost:4318/v1/metrics".into()),
            otlp_logs_url: Some("http://localhost:4318/v1/logs".into()),
        }
    );
}

#[test]
fn falls_back_cleanly_when_persisted_settings_are_invalid() {
    assert_eq!(parse_persisted_server_observability_settings("{"), PersistedObservabilitySettings::default());
}

fn with_text_generation(options: Value) -> Value {
    with(
        default_settings(),
        "textGenerationModelSelection",
        selection("codex", "gpt-5.4-mini", Some(options)),
    )
}

#[test]
fn replaces_text_generation_selection_when_provider_and_model_are_provided() {
    let current = with_text_generation(json!([{"id": "reasoningEffort", "value": "high"}, {"id": "fastMode", "value": true}]));
    let next = apply(
        &current,
        json!({"textGenerationModelSelection": {"instanceId": "codex", "model": "gpt-5.4-mini"}}),
    );
    assert_eq!(next["textGenerationModelSelection"], json!({"instanceId": "codex", "model": "gpt-5.4-mini"}));
}

#[test]
fn still_deep_merges_text_generation_selection_when_only_options_are_provided() {
    let current = with_text_generation(json!([{"id": "reasoningEffort", "value": "high"}, {"id": "fastMode", "value": true}]));
    let next = apply(
        &current,
        json!({"textGenerationModelSelection": {"options": [{"id": "fastMode", "value": false}]}}),
    );
    assert_eq!(
        next["textGenerationModelSelection"],
        json!({"instanceId": "codex", "model": "gpt-5.4-mini", "options": [
            {"id": "reasoningEffort", "value": "high"},
            {"id": "fastMode", "value": false}
        ]})
    );
}

#[test]
fn replaces_text_generation_selection_across_providers_without_leaking_stale_options() {
    let current = with_text_generation(json!([{"id": "reasoningEffort", "value": "high"}, {"id": "fastMode", "value": true}]));
    let next = apply(
        &current,
        json!({"textGenerationModelSelection": {"instanceId": "opencode", "model": "openai/gpt-5"}}),
    );
    assert_eq!(next["textGenerationModelSelection"], json!({"instanceId": "opencode", "model": "openai/gpt-5"}));
}

#[test]
fn accepts_array_based_text_generation_selection_patches() {
    let next = apply(
        &default_settings(),
        json!({"textGenerationModelSelection": {"instanceId": "opencode", "model": "openai/gpt-5", "options": [
            {"id": "variant", "value": "prod"}, {"id": "agent", "value": "build"}
        ]}}),
    );
    assert_eq!(
        next["textGenerationModelSelection"],
        json!({"instanceId": "opencode", "model": "openai/gpt-5", "options": [
            {"id": "variant", "value": "prod"}, {"id": "agent", "value": "build"}
        ]})
    );
}

#[test]
fn replaces_source_control_writer_selection_without_retaining_stale_options() {
    let current = with(
        default_settings(),
        "sourceControlWriterModelSelection",
        selection("codex", "gpt-5.4-mini", Some(json!([{"id": "reasoningEffort", "value": "high"}]))),
    );
    let next = apply(
        &current,
        json!({"sourceControlWriterModelSelection": {"instanceId": "opencode", "model": "openai/gpt-5"}}),
    );
    assert_eq!(
        next["sourceControlWriterModelSelection"],
        json!({"instanceId": "opencode", "model": "openai/gpt-5"})
    );
}

#[test]
fn clears_source_control_writer_selection_with_null() {
    let current = with(
        default_settings(),
        "sourceControlWriterModelSelection",
        selection("codex", "gpt-5.4-mini", None),
    );
    let next = apply(&current, json!({"sourceControlWriterModelSelection": null}));
    assert_eq!(next["sourceControlWriterModelSelection"], Value::Null);
}

#[test]
fn falls_back_from_a_disabled_source_control_writer_provider_without_clearing_its_selection() {
    let writer = selection("codex_writer", "gpt-5.4-mini", None);
    let settings = with(
        with(
            default_settings(),
            "providerInstances",
            json!({"codex_writer": {"driver": "codex", "enabled": false, "config": {}}}),
        ),
        "sourceControlWriterModelSelection",
        writer.clone(),
    );
    assert!(!is_model_selection_provider_enabled(&settings, &writer));
    assert_eq!(
        resolve_source_control_writer_model_selection(&settings, None),
        settings["textGenerationModelSelection"]
    );
    assert_eq!(settings["sourceControlWriterModelSelection"], writer);
}

#[test]
fn falls_back_from_an_unavailable_source_control_writer_provider() {
    let writer = selection("missing_writer", "missing-model", None);
    let settings = with(
        with(
            default_settings(),
            "providerInstances",
            json!({"missing_writer": {"driver": "missing-driver", "config": {}}}),
        ),
        "sourceControlWriterModelSelection",
        writer.clone(),
    );
    let unavailable = json!({
        "instanceId": "missing_writer",
        "driver": "missing-driver",
        "enabled": false,
        "installed": false,
        "version": null,
        "status": "disabled",
        "auth": {"status": "unknown"},
        "checkedAt": "2026-07-27T00:00:00.000Z",
        "availability": "unavailable",
        "unavailableReason": "This provider driver is not available in this build.",
        "models": [],
        "slashCommands": [],
        "skills": []
    });
    assert_eq!(
        resolve_source_control_writer_model_selection(&settings, Some(&[unavailable])),
        settings["textGenerationModelSelection"]
    );
    assert_eq!(settings["sourceControlWriterModelSelection"], writer);
}

#[test]
fn replaces_provider_instances_maps_so_omitted_instance_fields_are_cleared() {
    let current = with(
        default_settings(),
        "providerInstances",
        json!({"codex": {"driver": "codex", "displayName": "Codex Work", "accentColor": "#7c3aed", "enabled": true, "config": {"homePath": "~/.codex"}}}),
    );
    let next = apply(
        &current,
        json!({"providerInstances": {"codex": {"driver": "codex", "displayName": "Codex Work", "enabled": true, "config": {"homePath": "~/.codex"}}}}),
    );
    assert_eq!(
        next["providerInstances"]["codex"],
        json!({"driver": "codex", "displayName": "Codex Work", "enabled": true, "config": {"homePath": "~/.codex"}})
    );
}

#[test]
fn upserts_and_removes_usage_limit_sources_per_entry_so_concurrent_edits_cannot_clobber() {
    let source = |url: &str| json!({"kind": "cliproxy", "url": url, "managementKey": "secret", "enabled": true});
    let current = with(default_settings(), "usageLimitSources", json!({"cliproxy-a": source("http://a:8318")}));
    let added = apply(&current, json!({"usageLimitSources": {"cliproxy-b": source("http://b:8318")}}));
    let keys: Vec<_> = added["usageLimitSources"].as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["cliproxy-a", "cliproxy-b"]);
    let removed = apply(&added, json!({"usageLimitSources": {"cliproxy-a": null}}));
    let keys: Vec<_> = removed["usageLimitSources"].as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["cliproxy-b"]);
}

#[test]
fn replaces_and_removes_individual_usage_prices_without_clobbering_other_models() {
    let prices = json!({"inputCostPerMillionTokens": 2, "outputCostPerMillionTokens": 8});
    let current = apply(
        &default_settings(),
        json!({"usagePriceOverrides": {"example-model": {"inputCostPerMillionTokens": 2, "outputCostPerMillionTokens": 8, "cacheReadCostPerMillionTokens": 0.5}}}),
    );
    let added = apply(&current, json!({"usagePriceOverrides": {"other-model": prices}}));
    let replaced = apply(&added, json!({"usagePriceOverrides": {"example-model": prices}}));
    assert_eq!(replaced["usagePriceOverrides"], json!({"example-model": prices, "other-model": prices}));
    let removed = apply(&replaced, json!({"usagePriceOverrides": {"example-model": null}}));
    assert_eq!(removed["usagePriceOverrides"], json!({"other-model": prices}));
    assert_eq!(current["usagePriceOverrides"]["example-model"]["cacheReadCostPerMillionTokens"], json!(0.5));
}

#[test]
fn stores_background_activity_profiles_as_a_versioned_object_and_syncs_legacy_aliases() {
    let next = apply(
        &default_settings(),
        json!({"backgroundActivity": {"schemaVersion": 1, "profile": "battery-saver", "overrides": {}}}),
    );
    assert_eq!(
        next["backgroundActivity"],
        json!({"schemaVersion": 1, "profile": "battery-saver", "overrides": {}})
    );
    assert_eq!(next["backgroundActivityProfile"], json!("battery-saver"));
    assert_eq!(next["automaticGitFetchInterval"], json!(0));
    assert_eq!(next["providerHealthRefreshInterval"], json!(900_000));
}

#[test]
fn turns_legacy_interval_patches_into_custom_background_activity_overrides() {
    let next = apply(&default_settings(), json!({"automaticGitFetchInterval": 15_000}));
    assert_eq!(
        next["backgroundActivity"],
        json!({"schemaVersion": 1, "profile": "custom", "baseProfile": "balanced", "overrides": {"automaticGitFetchInterval": 15_000}})
    );
    let resolved = background::resolve_server(&next);
    assert_eq!(resolved.profile, "balanced");
    assert_eq!(resolved.automatic_git_fetch_interval, 15_000.0);
}

#[test]
fn preserves_legacy_background_activity_settings_when_applying_an_unrelated_patch() {
    let mut current = default_settings();
    current["backgroundActivityProfile"] = json!("performance");
    current["automaticGitFetchInterval"] = json!(7_000);
    current["providerHealthRefreshInterval"] = json!(240_000);
    let next = apply(&current, json!({"sourceControlWriterModelSelection": selection("codex", "gpt-5.4-mini", None)}));
    assert_eq!(
        next["backgroundActivity"],
        json!({"schemaVersion": 1, "profile": "custom", "baseProfile": "performance", "overrides": {
            "automaticGitFetchInterval": 7_000,
            "providerHealthRefreshInterval": 240_000
        }})
    );
    assert_eq!(next["backgroundActivityProfile"], json!("performance"));
    assert_eq!(next["automaticGitFetchInterval"], json!(7_000));
    assert_eq!(next["providerHealthRefreshInterval"], json!(240_000));
}

#[test]
fn does_not_reactivate_dormant_overrides_from_a_concrete_profile() {
    let current = with(
        default_settings(),
        "backgroundActivity",
        json!({"schemaVersion": 1, "profile": "battery-saver", "overrides": {"providerHealthRefreshInterval": 5_000}}),
    );
    let next = apply(&current, json!({"automaticGitFetchInterval": 15_000}));
    assert_eq!(
        next["backgroundActivity"],
        json!({"schemaVersion": 1, "profile": "custom", "baseProfile": "battery-saver", "overrides": {"automaticGitFetchInterval": 15_000}})
    );
}

#[test]
fn prefers_structured_background_activity_settings_over_legacy_aliases() {
    let next = apply(
        &default_settings(),
        json!({
            "backgroundActivity": {"schemaVersion": 1, "profile": "battery-saver", "overrides": {}},
            "automaticGitFetchInterval": 5_000,
            "backgroundActivityProfile": "performance"
        }),
    );
    assert_eq!(
        next["backgroundActivity"],
        json!({"schemaVersion": 1, "profile": "battery-saver", "overrides": {}})
    );
    assert_eq!(next["backgroundActivityProfile"], json!("battery-saver"));
    assert_eq!(next["automaticGitFetchInterval"], json!(0));
}

#[test]
fn reconciles_custom_background_activity_back_to_a_preset_when_overrides_match_the_preset() {
    let custom = apply(&default_settings(), json!({"automaticGitFetchInterval": 15_000}));
    let next = apply(&custom, json!({"automaticGitFetchInterval": 30_000}));
    assert_eq!(next["backgroundActivity"], json!({"schemaVersion": 1, "profile": "balanced", "overrides": {}}));
    assert_eq!(next["backgroundActivityProfile"], json!("balanced"));
    assert_eq!(next["automaticGitFetchInterval"], json!(30_000));
}

#[test]
fn drops_custom_overrides_that_duplicate_the_base_profile() {
    let next = apply(
        &default_settings(),
        json!({"backgroundActivity": {"schemaVersion": 1, "profile": "custom", "baseProfile": "balanced", "overrides": {"automaticGitFetchInterval": 30_000}}}),
    );
    assert_eq!(next["backgroundActivity"], json!({"schemaVersion": 1, "profile": "balanced", "overrides": {}}));
}

#[test]
fn replaces_the_complete_background_override_record() {
    let current = apply(
        &default_settings(),
        json!({"backgroundActivity": {"schemaVersion": 1, "profile": "custom", "baseProfile": "balanced", "overrides": {
            "automaticGitFetchInterval": 15_000,
            "providerHealthRefreshInterval": 180_000
        }}}),
    );
    let next = apply(&current, json!({"backgroundActivity": {"overrides": {"automaticGitFetchInterval": 10_000}}}));
    assert_eq!(
        next["backgroundActivity"],
        json!({"schemaVersion": 1, "profile": "custom", "baseProfile": "balanced", "overrides": {"automaticGitFetchInterval": 10_000}})
    );
}

#[test]
fn keeps_interval_overrides_supplied_with_a_profile_patch() {
    let next = apply(
        &default_settings(),
        json!({"backgroundActivityProfile": "performance", "automaticGitFetchInterval": 0, "providerHealthRefreshInterval": 240_000}),
    );
    assert_eq!(
        next["backgroundActivity"],
        json!({"schemaVersion": 1, "profile": "custom", "baseProfile": "performance", "overrides": {
            "automaticGitFetchInterval": 0,
            "providerHealthRefreshInterval": 240_000
        }})
    );
}

#[test]
fn ignores_overrides_attached_to_a_concrete_background_profile() {
    let settings = with(
        default_settings(),
        "backgroundActivity",
        json!({"schemaVersion": 1, "profile": "balanced", "overrides": {"pauseWhenOnBattery": true}}),
    );
    assert!(!background::resolve_server(&settings).pause_when_on_battery);
}
