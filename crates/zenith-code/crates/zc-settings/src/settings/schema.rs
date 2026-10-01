//! `ServerSettings` / `ServerSettingsPatch` decoding with the TypeScript schema's semantics.
//!
//! The settings logic works on the **encoded** JSON (`serde_json::Value`, insertion-ordered),
//! the way the TS service works on plain objects, because the written file must be
//! byte-identical: struct keys in declaration order, record keys in insertion order. The
//! generated `zc_contracts::ServerSettings` gives the structure, the decoding defaults and the
//! declaration order; this module adds what the generated types leave out:
//!
//! - the string transformations: `TrimmedString` / `TrimmedNonEmptyString` (trim, and reject an
//!   empty result) and the binary-path fallback (`""` → `"codex"`), as path rules extracted
//!   from the Effect AST (see [`SETTINGS_STRING_RULES`]);
//! - the legacy `ModelSelection` shapes (`{provider, model}`, object-shaped `options`);
//! - `ForwardCompatibleNullable` members that the generated type keeps as raw JSON
//!   (`environmentIcon`, `worktreeSubmodules`: an unknown value decodes as `null`);
//! - record key order, which `BTreeMap` loses: restored from the input.

use serde_json::{Map, Value};
use zc_contracts::{EnvironmentMachineKind, ServerSettings, ServerSettingsPatch, WorktreeSubmodules};

use crate::js::{js_key_order, js_trim};

/// What a string transformation of the schema does on decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringRule {
    /// `TrimmedString`.
    Trim,
    /// `TrimmedNonEmptyString`: trimmed, and empty is a decode failure.
    TrimNonEmpty,
    /// The legacy binary-path settings: trimmed, empty falls back to the executable name.
    TrimOr(&'static str),
}

/// The `ServerSettings` string rules, generated from the Effect AST by probing every string
/// node with `" a "`, `""` and `" "` (scratch script `trimpaths.ts`, see the crate docs).
///
/// Path syntax: `.`-separated keys; `*` is every value of an object, `[]` every element of an
/// array, `<key>` the keys of a record (renamed in place).
pub const SETTINGS_STRING_RULES: &[(&str, StringRule)] = &[
    ("projectAgentBrowserAccessOverrides.<key>", StringRule::TrimNonEmpty),
    ("defaultProjectScripts.[].id", StringRule::TrimNonEmpty),
    ("defaultProjectScripts.[].name", StringRule::TrimNonEmpty),
    ("defaultProjectScripts.[].command", StringRule::TrimNonEmpty),
    ("defaultProjectScripts.[].previewUrl", StringRule::TrimNonEmpty),
    ("projectScriptOverrides.<key>", StringRule::TrimNonEmpty),
    ("projectScriptOverrides.*.[].id", StringRule::TrimNonEmpty),
    ("projectScriptOverrides.*.[].name", StringRule::TrimNonEmpty),
    ("projectScriptOverrides.*.[].command", StringRule::TrimNonEmpty),
    ("projectScriptOverrides.*.[].previewUrl", StringRule::TrimNonEmpty),
    ("projectAutoPullOverrides.<key>", StringRule::TrimNonEmpty),
    ("projectSettingsOverrides.<key>", StringRule::TrimNonEmpty),
    ("projectSettingsOverrides.*.defaultProjectScripts.[].id", StringRule::TrimNonEmpty),
    ("projectSettingsOverrides.*.defaultProjectScripts.[].name", StringRule::TrimNonEmpty),
    ("projectSettingsOverrides.*.defaultProjectScripts.[].command", StringRule::TrimNonEmpty),
    ("projectSettingsOverrides.*.defaultProjectScripts.[].previewUrl", StringRule::TrimNonEmpty),
    ("projectSettingsOverrides.*.sourceControlWritingStyle.customInstructions", StringRule::Trim),
    ("deviceHosts.[].id", StringRule::TrimNonEmpty),
    ("deviceHosts.[].label", StringRule::TrimNonEmpty),
    ("deviceHosts.[].target", StringRule::TrimNonEmpty),
    ("deviceHosts.[].identityFile", StringRule::TrimNonEmpty),
    ("addProjectBaseDirectory", StringRule::Trim),
    ("sourceControlWritingStyle.customInstructions", StringRule::Trim),
    ("providers.codex.binaryPath", StringRule::TrimOr("codex")),
    ("providers.codex.homePath", StringRule::Trim),
    ("providers.codex.shadowHomePath", StringRule::Trim),
    ("providers.codex.launchArgs", StringRule::Trim),
    ("providers.claudeAgent.binaryPath", StringRule::TrimOr("claude")),
    ("providers.claudeAgent.homePath", StringRule::Trim),
    ("providers.claudeAgent.autoCompactWindow", StringRule::Trim),
    ("providers.cursor.binaryPath", StringRule::TrimOr("cursor-agent")),
    ("providers.cursor.apiEndpoint", StringRule::Trim),
    ("providers.grok.binaryPath", StringRule::TrimOr("grok")),
    ("providers.opencode.binaryPath", StringRule::TrimOr("opencode")),
    ("providers.opencode.serverUrl", StringRule::Trim),
    ("providers.opencode.serverPassword", StringRule::Trim),
    ("providers.antigravity.apiKey", StringRule::Trim),
    ("providers.antigravity.gcpProject", StringRule::Trim),
    ("providers.antigravity.gcpLocation", StringRule::Trim),
    ("providers.antigravity.binaryPath", StringRule::Trim),
    ("providers.*.customModels.[].slug", StringRule::TrimNonEmpty),
    ("providers.*.customModels.[].name", StringRule::TrimNonEmpty),
    ("providers.*.customModels.[].capabilities.optionDescriptors.[].id", StringRule::TrimNonEmpty),
    ("providers.*.customModels.[].capabilities.optionDescriptors.[].label", StringRule::TrimNonEmpty),
    (
        "providers.*.customModels.[].capabilities.optionDescriptors.[].description",
        StringRule::TrimNonEmpty,
    ),
    (
        "providers.*.customModels.[].capabilities.optionDescriptors.[].options.[].id",
        StringRule::TrimNonEmpty,
    ),
    (
        "providers.*.customModels.[].capabilities.optionDescriptors.[].options.[].label",
        StringRule::TrimNonEmpty,
    ),
    (
        "providers.*.customModels.[].capabilities.optionDescriptors.[].options.[].description",
        StringRule::TrimNonEmpty,
    ),
    (
        "providers.*.customModels.[].capabilities.optionDescriptors.[].currentValue",
        StringRule::TrimNonEmpty,
    ),
    (
        "providers.*.customModels.[].capabilities.optionDescriptors.[].promptInjectedValues.[]",
        StringRule::TrimNonEmpty,
    ),
    ("providerInstances.<key>", StringRule::TrimNonEmpty),
    ("providerInstances.*.driver", StringRule::TrimNonEmpty),
    ("providerInstances.*.displayName", StringRule::TrimNonEmpty),
    ("providerInstances.*.accentColor", StringRule::TrimNonEmpty),
    ("providerInstances.*.environment.[].name", StringRule::TrimNonEmpty),
    ("observability.otlpTracesUrl", StringRule::Trim),
    ("observability.otlpMetricsUrl", StringRule::Trim),
    ("observability.otlpLogsUrl", StringRule::Trim),
    ("bitbucket.email", StringRule::Trim),
    ("bitbucket.accessToken", StringRule::Trim),
    ("bitbucket.apiToken", StringRule::Trim),
    ("usageLimitSources.*.label", StringRule::TrimNonEmpty),
    ("usageLimitSources.*.url", StringRule::TrimNonEmpty),
    ("usageLimitSources.*.managementKey", StringRule::Trim),
    ("usagePriceOverrides.<key>", StringRule::TrimNonEmpty),
];

/// The decoding defaults (`withDecodingDefault`) below the top level of `ServerSettings`, as
/// `(parent path, key, encoded default)`, top-down. The generated types only apply the top-level
/// ones (nested ones decode as absent), so they are filled in before the typed decode. Extracted
/// from the Effect AST by decoding each property signature from `{}` (scratch script
/// `defaults.ts`, see docs/zenith-code/settings.md).
pub const SETTINGS_NESTED_DEFAULTS: &[(&str, &str, &str)] = &[
    ("storageCleanup", "worktreeAfterDays", r#"null"#),
    ("storageCleanup", "worktreeOnMerge", r#"false"#),
    ("storageCleanup", "worktreeOnDelete", r#"false"#),
    ("storageCleanup", "worktreeUnchanged", r#"false"#),
    ("storageCleanup", "browserArtifactsAfterDays", r#"null"#),
    ("storageCleanup", "logsAfterDays", r#"null"#),
    ("projectSettingsOverrides.*.sourceControlWritingStyle", "mode", r#""repo_conventions""#),
    ("projectSettingsOverrides.*.sourceControlWritingStyle", "customInstructions", r#""""#),
    (
        "projectSettingsOverrides.*.sourceControlWritingStyle",
        "followChangeRequestTemplates",
        r#"true"#,
    ),
    ("backgroundActivity", "schemaVersion", r#"1"#),
    ("backgroundActivity", "profile", r#""balanced""#),
    ("backgroundActivity", "overrides", r#"{}"#),
    ("sourceControlWritingStyle", "mode", r#""repo_conventions""#),
    ("sourceControlWritingStyle", "customInstructions", r#""""#),
    ("sourceControlWritingStyle", "followChangeRequestTemplates", r#"true"#),
    (
        "providers",
        "codex",
        r#"{"enabled":true,"binaryPath":"codex","homePath":"","shadowHomePath":"","launchArgs":"","customModels":[]}"#,
    ),
    ("providers.codex", "enabled", r#"true"#),
    ("providers.codex", "binaryPath", r#""codex""#),
    ("providers.codex", "homePath", r#""""#),
    ("providers.codex", "shadowHomePath", r#""""#),
    ("providers.codex", "launchArgs", r#""""#),
    ("providers.codex", "customModels", r#"[]"#),
    (
        "providers",
        "claudeAgent",
        r#"{"enabled":true,"binaryPath":"claude","homePath":"","customModels":[],"launchArgs":"","autoCompactWindow":""}"#,
    ),
    ("providers.claudeAgent", "enabled", r#"true"#),
    ("providers.claudeAgent", "binaryPath", r#""claude""#),
    ("providers.claudeAgent", "homePath", r#""""#),
    ("providers.claudeAgent", "customModels", r#"[]"#),
    ("providers.claudeAgent", "launchArgs", r#""""#),
    ("providers.claudeAgent", "autoCompactWindow", r#""""#),
    (
        "providers",
        "cursor",
        r#"{"enabled":false,"binaryPath":"cursor-agent","apiEndpoint":"","customModels":[]}"#,
    ),
    ("providers.cursor", "enabled", r#"false"#),
    ("providers.cursor", "binaryPath", r#""cursor-agent""#),
    ("providers.cursor", "apiEndpoint", r#""""#),
    ("providers.cursor", "customModels", r#"[]"#),
    ("providers", "grok", r#"{"enabled":false,"binaryPath":"grok","customModels":[]}"#),
    ("providers.grok", "enabled", r#"false"#),
    ("providers.grok", "binaryPath", r#""grok""#),
    ("providers.grok", "customModels", r#"[]"#),
    (
        "providers",
        "opencode",
        r#"{"enabled":false,"binaryPath":"opencode","serverUrl":"","serverPassword":"","customModels":[]}"#,
    ),
    ("providers.opencode", "enabled", r#"false"#),
    ("providers.opencode", "binaryPath", r#""opencode""#),
    ("providers.opencode", "serverUrl", r#""""#),
    ("providers.opencode", "serverPassword", r#""""#),
    ("providers.opencode", "customModels", r#"[]"#),
    (
        "providers",
        "antigravity",
        r#"{"enabled":false,"authMethod":"oauth-personal","apiKey":"","gcpProject":"","gcpLocation":"","binaryPath":"","customModels":[]}"#,
    ),
    ("providers.antigravity", "enabled", r#"false"#),
    ("providers.antigravity", "authMethod", r#""oauth-personal""#),
    ("providers.antigravity", "apiKey", r#""""#),
    ("providers.antigravity", "gcpProject", r#""""#),
    ("providers.antigravity", "gcpLocation", r#""""#),
    ("providers.antigravity", "binaryPath", r#""""#),
    ("providers.antigravity", "customModels", r#"[]"#),
    ("providerInstances.*.environment.[]", "value", r#""""#),
    ("providerInstances.*.environment.[]", "sensitive", r#"false"#),
    ("observability", "otlpTracesUrl", r#""""#),
    ("observability", "otlpMetricsUrl", r#""""#),
    ("observability", "otlpLogsUrl", r#""""#),
    ("bitbucket", "email", r#""""#),
    ("bitbucket", "accessToken", r#""""#),
    ("bitbucket", "apiToken", r#""""#),
    ("usageLimitSources.*", "managementKey", r#""""#),
    ("usageLimitSources.*", "enabled", r#"true"#),
];

/// The same for `ServerSettingsPatch` (whose own fields are all optional).
pub const PATCH_NESTED_DEFAULTS: &[(&str, &str, &str)] = &[
    ("projectSettingsOverrides.*.sourceControlWritingStyle", "mode", r#""repo_conventions""#),
    ("projectSettingsOverrides.*.sourceControlWritingStyle", "customInstructions", r#""""#),
    (
        "projectSettingsOverrides.*.sourceControlWritingStyle",
        "followChangeRequestTemplates",
        r#"true"#,
    ),
    ("providerInstances.*.environment.[]", "value", r#""""#),
    ("providerInstances.*.environment.[]", "sensitive", r#"false"#),
    ("usageLimitSources.*", "managementKey", r#""""#),
    ("usageLimitSources.*", "enabled", r#"true"#),
];

/// Every `ModelSelection` of `ServerSettings` (and of the patch: same paths, plus the patch's
/// partial `textGenerationModelSelection`, which has the same fields).
pub const MODEL_SELECTION_PATHS: &[&str] = &[
    "defaultModelSelection",
    "textGenerationModelSelection",
    "sourceControlWriterModelSelection",
    "projectSettingsOverrides.*.defaultModelSelection",
    "projectSettingsOverrides.*.textGenerationModelSelection",
    "projectSettingsOverrides.*.sourceControlWriterModelSelection",
];

/// The `Schema.Record` fields of `ServerSettings` (and of the patch): their key order is the
/// input's, which the generated `BTreeMap`s do not keep.
pub const RECORD_FIELDS: &[&str] = &[
    "projectAgentBrowserAccessOverrides",
    "projectScriptOverrides",
    "projectAutoPullOverrides",
    "projectSettingsOverrides",
    "providerInstances",
    "usageLimitSources",
    "usagePriceOverrides",
];

/// A value that does not decode: TS would fail the whole `Schema.decode`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct DecodeIssue {
    pub message: String,
}

impl DecodeIssue {
    fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

/// `DEFAULT_SERVER_SETTINGS`, encoded.
pub fn default_settings() -> Value {
    static DEFAULTS: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    DEFAULTS
        .get_or_init(|| {
            let decoded: ServerSettings = serde_json::from_value(Value::Object(Map::new())).expect("ServerSettings decodes from {}");
            serde_json::to_value(decoded).expect("ServerSettings encodes")
        })
        .clone()
}

/// `Schema.decodeUnknown(ServerSettings)` followed by `Schema.encode`: the canonical encoded
/// settings for any input (the TS service only ever holds decoded values, and every write
/// encodes them).
pub fn decode_settings(raw: &Value) -> Result<Value, DecodeIssue> {
    let Value::Object(_) = raw else {
        return Err(DecodeIssue::new("Expected an object"));
    };
    let mut input = raw.clone();
    fill_nested_defaults(&mut input, SETTINGS_NESTED_DEFAULTS)?;
    prepare(&mut input, SETTINGS_STRING_RULES, &[])?;
    let typed: ServerSettings = serde_json::from_value(input.clone()).map_err(|error| DecodeIssue::new(error.to_string()))?;
    let mut encoded = serde_json::to_value(typed).map_err(|error| DecodeIssue::new(error.to_string()))?;
    restore_record_order(&mut encoded, &input);
    normalize_forward_compatible(&mut encoded);
    Ok(encoded)
}

/// `Schema.decodeUnknown(ServerSettingsPatch)`, encoded back: the patch the TS service
/// receives (trimmed, record values with their decoding defaults).
pub fn decode_patch(raw: &Value) -> Result<Value, DecodeIssue> {
    let Value::Object(_) = raw else {
        return Err(DecodeIssue::new("Expected an object"));
    };
    let mut input = raw.clone();
    fill_nested_defaults(&mut input, PATCH_NESTED_DEFAULTS)?;
    prepare(&mut input, SETTINGS_STRING_RULES, &[("providers.claudeAgent.launchArgs", StringRule::Trim)])?;
    let typed: ServerSettingsPatch = serde_json::from_value(input.clone()).map_err(|error| DecodeIssue::new(error.to_string()))?;
    let mut encoded = serde_json::to_value(typed).map_err(|error| DecodeIssue::new(error.to_string()))?;
    restore_record_order(&mut encoded, &input);
    Ok(encoded)
}

/// Insert each missing nested decoding default (only where the parent is an object).
fn fill_nested_defaults(input: &mut Value, defaults: &[(&str, &str, &str)]) -> Result<(), DecodeIssue> {
    for (parent, key, default) in defaults {
        let segments: Vec<&str> = parent.split('.').collect();
        for_each_at(input, &segments, &mut |value| {
            if let Value::Object(map) = value {
                if !map.contains_key(*key) {
                    let default: Value = serde_json::from_str(default).expect("valid default");
                    map.insert((*key).to_owned(), default);
                }
            }
            Ok(())
        })?;
    }
    Ok(())
}

/// The steps before the typed decode: legacy model selections, then the string rules. The
/// patch's string fields are plain `TrimmedString`s, so the binary-path fallbacks become trims
/// there (`extra` adds the patch-only rules).
fn prepare(input: &mut Value, rules: &[(&str, StringRule)], extra: &[(&str, StringRule)]) -> Result<(), DecodeIssue> {
    let is_patch = !extra.is_empty();
    for path in MODEL_SELECTION_PATHS {
        let segments: Vec<&str> = path.split('.').collect();
        // The patch's `textGenerationModelSelection` is `ModelSelectionPatch`: partial, and
        // without the `provider` promotion; only its `options` accept the legacy object.
        let partial = is_patch && *path == "textGenerationModelSelection";
        for_each_at(input, &segments, &mut |value| {
            if partial {
                if let Some(options) = value.get_mut("options") {
                    let coerced = match &*options {
                        Value::Object(record) => Some(coerce_legacy_options(record)),
                        _ => None,
                    };
                    if let Some(coerced) = coerced {
                        *options = coerced;
                    }
                }
            } else {
                upgrade_legacy_model_selection(value);
            }
            Ok(())
        })?;
    }
    for (path, rule) in rules.iter().chain(extra) {
        let rule = match (is_patch, *rule) {
            (true, StringRule::TrimOr(_)) => StringRule::Trim,
            (_, rule) => rule,
        };
        apply_string_rule(input, path, rule)?;
    }
    for path in MODEL_SELECTION_PATHS {
        for field in ["instanceId", "model", "options.[].id", "options.[].value"] {
            apply_string_rule(input, &format!("{path}.{field}"), StringRule::TrimNonEmpty)?;
        }
    }
    Ok(())
}

/// The `ModelSelection` pre-decoding transform: `{provider, model}` becomes
/// `{instanceId: provider, model}`, and object-shaped `options` (pre-migration-026) become the
/// canonical array.
fn upgrade_legacy_model_selection(value: &mut Value) {
    let Value::Object(map) = value else {
        return;
    };
    let instance_id = match map.get("instanceId") {
        Some(id) if !id.is_null() => Some(id.clone()),
        _ => match map.get("provider") {
            Some(Value::String(provider)) => Some(Value::String(provider.clone())),
            _ => None,
        },
    };
    let mut next = Map::new();
    if let Some(id) = instance_id {
        next.insert("instanceId".into(), id);
    }
    if let Some(model) = map.get("model") {
        next.insert("model".into(), model.clone());
    }
    if let Some(options) = map.get("options") {
        let options = match options {
            Value::Object(record) => coerce_legacy_options(record),
            other => other.clone(),
        };
        next.insert("options".into(), options);
    }
    *map = next;
}

/// `coerceLegacyOptionsObjectToArray`.
fn coerce_legacy_options(record: &Map<String, Value>) -> Value {
    let mut entries = Vec::new();
    for (raw_key, raw_value) in record {
        let id = js_trim(raw_key);
        if id.is_empty() {
            continue;
        }
        match raw_value {
            Value::String(text) => {
                let trimmed = js_trim(text);
                if !trimmed.is_empty() {
                    entries.push(serde_json::json!({"id": id, "value": trimmed}));
                }
            }
            Value::Bool(flag) => entries.push(serde_json::json!({"id": id, "value": flag})),
            _ => {}
        }
    }
    Value::Array(entries)
}

/// Apply one rule at every match of `path` (strings only; other types are the typed decode's
/// business).
pub fn apply_string_rule(value: &mut Value, path: &str, rule: StringRule) -> Result<(), DecodeIssue> {
    let segments: Vec<&str> = path.split('.').collect();
    if segments.last() == Some(&"<key>") {
        let parent = &segments[..segments.len() - 1];
        return for_each_at(value, parent, &mut |record| {
            if let Value::Object(map) = record {
                let entries = std::mem::take(map);
                for (key, item) in entries {
                    let key = transform(&key, rule).ok_or_else(|| DecodeIssue::new(format!("Invalid value at {path}: empty key")))?;
                    map.insert(key, item);
                }
            }
            Ok(())
        });
    }
    for_each_at(value, &segments, &mut |target| {
        if let Value::String(text) = target {
            *text = transform(text, rule).ok_or_else(|| DecodeIssue::new(format!("Invalid value at {path}: empty string")))?;
        }
        Ok(())
    })
}

fn transform(text: &str, rule: StringRule) -> Option<String> {
    let trimmed = js_trim(text);
    match rule {
        StringRule::Trim => Some(trimmed.to_owned()),
        StringRule::TrimNonEmpty => (!trimmed.is_empty()).then(|| trimmed.to_owned()),
        StringRule::TrimOr(fallback) => Some(if trimmed.is_empty() { fallback.to_owned() } else { trimmed.to_owned() }),
    }
}

/// Visit every value matching `segments` (see [`SETTINGS_STRING_RULES`] for the syntax).
fn for_each_at(value: &mut Value, segments: &[&str], visit: &mut dyn FnMut(&mut Value) -> Result<(), DecodeIssue>) -> Result<(), DecodeIssue> {
    let Some((head, rest)) = segments.split_first() else {
        return visit(value);
    };
    match (*head, value) {
        ("*", Value::Object(map)) => {
            for item in map.values_mut() {
                for_each_at(item, rest, visit)?;
            }
        }
        ("[]", Value::Array(items)) => {
            for item in items {
                for_each_at(item, rest, visit)?;
            }
        }
        (key, Value::Object(map)) => {
            if let Some(item) = map.get_mut(key) {
                for_each_at(item, rest, visit)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Give each record field of `encoded` the key order it has in `input` (JS enumeration order).
fn restore_record_order(encoded: &mut Value, input: &Value) {
    for field in RECORD_FIELDS {
        let (Some(Value::Object(target)), Some(Value::Object(source))) = (encoded.get_mut(*field), input.get(*field)) else {
            continue;
        };
        let mut ordered = Map::new();
        for key in source.keys() {
            if let Some(item) = target.remove(key) {
                ordered.insert(key.clone(), item);
            }
        }
        // Keys the decode produced that the input did not have (none today) keep their place
        // after the input's.
        ordered.append(target);
        *target = js_key_order(ordered);
    }
}

/// `ForwardCompatibleNullable(X)`: a value this build does not know decodes as `null`.
fn normalize_forward_compatible(encoded: &mut Value) {
    let Value::Object(map) = encoded else {
        return;
    };
    if let Some(icon) = map.get_mut("environmentIcon") {
        if !icon.is_null() && serde_json::from_value::<EnvironmentMachineKind>(icon.clone()).is_err() {
            *icon = Value::Null;
        }
    }
    if let Some(submodules) = map.get_mut("worktreeSubmodules") {
        if !submodules.is_null() && serde_json::from_value::<WorktreeSubmodules>(submodules.clone()).is_err() {
            *submodules = Value::Null;
        }
    }
}

/// `Schema.decodeUnknown(ServerSettings)` into the generated type (canonicalizing first).
pub fn to_typed_settings(encoded: &Value) -> Result<ServerSettings, DecodeIssue> {
    serde_json::from_value(encoded.clone()).map_err(|error| DecodeIssue::new(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_match_the_contract() {
        let defaults = default_settings();
        assert_eq!(defaults["responseStreamingMode"], json!("paragraph"));
        assert_eq!(defaults["providers"]["codex"]["binaryPath"], json!("codex"));
        assert_eq!(defaults["automaticGitFetchInterval"], json!(30000));
        assert_eq!(
            defaults["textGenerationModelSelection"],
            json!({"instanceId": "codex", "model": "gpt-6-luna", "options": [{"id": "reasoningEffort", "value": "low"}]})
        );
        assert!(defaults.get("defaultThreadEnvMode").is_none());
        let keys: Vec<_> = defaults.as_object().unwrap().keys().take(3).cloned().collect();
        assert_eq!(keys, ["worktreeCleanup", "storageCleanup", "responseStreamingMode"]);
    }

    #[test]
    fn decodes_legacy_object_shaped_model_selection_options() {
        let decoded = decode_settings(&json!({
            "textGenerationModelSelection": {
                "provider": "codex",
                "model": "gpt-5.4-mini",
                "options": {"reasoningEffort": "low"}
            }
        }))
        .unwrap();
        assert_eq!(
            decoded["textGenerationModelSelection"],
            json!({"instanceId": "codex", "model": "gpt-5.4-mini", "options": [{"id": "reasoningEffort", "value": "low"}]})
        );
    }

    #[test]
    fn trims_and_falls_back_like_the_schema() {
        let decoded = decode_settings(&json!({
            "providers": {"codex": {"binaryPath": "   ", "homePath": " ~/.codex "}},
            "observability": {"otlpTracesUrl": " http://x "},
            "providerInstances": {"b": {"driver": "codex"}, "a": {"driver": " claudeAgent "}}
        }))
        .unwrap();
        assert_eq!(decoded["providers"]["codex"]["binaryPath"], json!("codex"));
        assert_eq!(decoded["providers"]["codex"]["homePath"], json!("~/.codex"));
        assert_eq!(decoded["observability"]["otlpTracesUrl"], json!("http://x"));
        let keys: Vec<_> = decoded["providerInstances"].as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["b", "a"]);
        assert_eq!(decoded["providerInstances"]["a"]["driver"], json!("claudeAgent"));
    }

    #[test]
    fn rejects_empty_trimmed_non_empty_strings() {
        assert!(decode_settings(&json!({"usageLimitSources": {"hub": {"kind": "cliproxy", "url": "  "}}})).is_err());
        assert!(decode_settings(&json!({"enableAgentBrowserAccess": "yes"})).is_err());
        assert!(decode_settings(&json!([])).is_err());
    }

    #[test]
    fn unknown_forward_compatible_values_decode_as_null() {
        let decoded = decode_settings(&json!({"environmentIcon": "spaceship", "worktreeSubmodules": "none"})).unwrap();
        assert_eq!(decoded["environmentIcon"], Value::Null);
        assert_eq!(decoded["worktreeSubmodules"], json!("none"));
    }

    #[test]
    fn patches_keep_partial_shapes() {
        let patch = decode_patch(&json!({
            "providers": {"claudeAgent": {"launchArgs": " --x "}},
            "usageLimitSources": {"hub": {"kind": "cliproxy", "url": "http://hub"}, "old": null},
            "textGenerationModelSelection": {"options": [{"id": "fastMode", "value": false}]}
        }))
        .unwrap();
        assert_eq!(patch["providers"], json!({"claudeAgent": {"launchArgs": "--x"}}));
        assert_eq!(
            patch["usageLimitSources"],
            json!({"hub": {"kind": "cliproxy", "url": "http://hub", "managementKey": "", "enabled": true}, "old": null})
        );
        assert_eq!(patch["textGenerationModelSelection"], json!({"options": [{"id": "fastMode", "value": false}]}));
    }
}
