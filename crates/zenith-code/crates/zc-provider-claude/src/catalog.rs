//! `provider/ClaudeModelCatalog.ts` + `ClaudeModelManifest.ts`: the Claude models of the model
//! manifest, their Claude Code runtime profile (effort map, model suffixes, context windows) and
//! their CLI-version compatibility.
//!
//! The manifest itself (fetching, caching, `ModelManifestSchema`) belongs to the provider core
//! (WP-12); this module reads the `providers.claudeAgent` section of a manifest JSON
//! ([`ClaudeModelCatalog::from_manifest`]) and falls back to the manifest bundled with the TS
//! server, exactly like `resolveClaudeModelCatalog`.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::sync::OnceLock;

use serde_json::{Map, Value};
use zc_contracts::ModelSelection;

use crate::model_options::{
    find_descriptor, provider_option_current_value, provider_option_descriptors, read_custom_model_entries, selection_string_option, selections_of, OptionValue,
};
use crate::semver::{compare_semver_versions, parse_semver};

/// The manifest the TS server ships (`provider/model-manifest.json`).
pub const BUNDLED_MODEL_MANIFEST_JSON: &str = include_str!("../../../../../code/apps/server/src/provider/model-manifest.json");

/// `ClaudeCodeProfile`: how a model's options reach Claude Code.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaudeCodeProfile {
    /// Effort option id → the `--effort` value (`None`: not an API effort, e.g. `ultrathink`).
    pub effort_map: Option<BTreeMap<String, Option<String>>>,
    /// Option id → option value → suffix appended to the model id (`[1m]`).
    pub model_suffixes: Option<Vec<(String, BTreeMap<String, String>)>>,
    pub context_window_tokens: Option<BTreeMap<String, f64>>,
    pub fixed_context_window_tokens: Option<f64>,
}

/// `ClaudeCodeCompatibility`: the CLI versions a model needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeCodeCompatibility {
    pub min_version: Option<String>,
    pub max_version_exclusive: Option<String>,
}

/// `ClaudeCatalogModel`.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeCatalogModel {
    /// The `ServerProviderModel` JSON (slug, name, aliases, capabilities, …).
    pub model: Value,
    pub runtime: ClaudeCodeProfile,
    pub compatibility: ClaudeCodeCompatibility,
}

impl ClaudeCatalogModel {
    pub fn slug(&self) -> &str {
        self.model.get("slug").and_then(Value::as_str).unwrap_or_default()
    }

    pub fn name(&self) -> &str {
        self.model.get("name").and_then(Value::as_str).unwrap_or_default()
    }

    fn aliases(&self) -> Vec<&str> {
        self.model
            .get("aliases")
            .and_then(Value::as_array)
            .map(|aliases| aliases.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }

    /// `capabilities`, `{optionDescriptors: []}` when null.
    pub fn capabilities(&self) -> Value {
        match self.model.get("capabilities") {
            Some(Value::Object(caps)) => Value::Object(caps.clone()),
            _ => empty_capabilities(),
        }
    }
}

fn empty_capabilities() -> Value {
    serde_json::json!({ "optionDescriptors": [] })
}

/// `ClaudeModelCatalog`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaudeModelCatalog {
    pub models: Vec<ClaudeCatalogModel>,
}

fn is_trimmed_non_empty(value: &str) -> bool {
    !value.trim().is_empty()
}

/// `decodeClaudeProfileAdapter(adapter ?? {})`.
fn decode_profile_adapter(adapter: Option<&Value>) -> Option<ClaudeCodeProfile> {
    let adapter = match adapter {
        None | Some(Value::Null) => return Some(ClaudeCodeProfile::default()),
        Some(Value::Object(object)) => object,
        Some(_) => return None,
    };
    let Some(profile) = adapter.get("claudeCode") else {
        return Some(ClaudeCodeProfile::default());
    };
    let profile = profile.as_object()?;
    let mut result = ClaudeCodeProfile::default();
    if let Some(effort_map) = profile.get("effortMap") {
        let mut map = BTreeMap::new();
        for (key, value) in effort_map.as_object()? {
            if !is_trimmed_non_empty(key) {
                return None;
            }
            let mapped = match value {
                Value::Null => None,
                Value::String(s) if is_trimmed_non_empty(s) => Some(s.trim().to_string()),
                _ => return None,
            };
            map.insert(key.trim().to_string(), mapped);
        }
        result.effort_map = Some(map);
    }
    if let Some(suffixes) = profile.get("modelSuffixes") {
        let mut list = Vec::new();
        for (option_id, values) in suffixes.as_object()? {
            let mut map = BTreeMap::new();
            for (value, suffix) in values.as_object()? {
                let suffix = suffix.as_str().filter(|s| is_trimmed_non_empty(s))?;
                map.insert(value.trim().to_string(), suffix.trim().to_string());
            }
            list.push((option_id.trim().to_string(), map));
        }
        result.model_suffixes = Some(list);
    }
    if let Some(tokens) = profile.get("contextWindowTokens") {
        let mut map = BTreeMap::new();
        for (key, value) in tokens.as_object()? {
            map.insert(key.trim().to_string(), value.as_f64()?);
        }
        result.context_window_tokens = Some(map);
    }
    if let Some(fixed) = profile.get("fixedContextWindowTokens") {
        result.fixed_context_window_tokens = Some(fixed.as_f64()?);
    }
    Some(result)
}

/// `decodeClaudeModelAdapter(adapter ?? {})`.
fn decode_model_adapter(adapter: Option<&Value>) -> Option<ClaudeCodeCompatibility> {
    let adapter = match adapter {
        None | Some(Value::Null) => return Some(ClaudeCodeCompatibility::default()),
        Some(Value::Object(object)) => object,
        Some(_) => return None,
    };
    let Some(compat) = adapter.get("claudeCode") else {
        return Some(ClaudeCodeCompatibility::default());
    };
    let compat = compat.as_object()?;
    let version = |key: &str| -> Option<Option<String>> {
        match compat.get(key) {
            None => Some(None),
            Some(Value::String(v)) if is_trimmed_non_empty(v) && parse_semver(v.trim()).is_some() => Some(Some(v.trim().to_string())),
            Some(_) => None,
        }
    };
    let min_version = version("minVersion")?;
    let max_version_exclusive = version("maxVersionExclusive")?;
    if let (Some(min), Some(max)) = (&min_version, &max_version_exclusive) {
        if compare_semver_versions(min, max) != Ordering::Less {
            return None;
        }
    }
    Some(ClaudeCodeCompatibility {
        min_version,
        max_version_exclusive,
    })
}

/// `hasValidClaudeManifestAdapters`.
pub fn has_valid_claude_manifest_adapters(manifest: &Value) -> bool {
    let Some(catalog) = manifest.get("providers").and_then(|p| p.get("claudeAgent")) else {
        return true;
    };
    let profiles_ok = catalog
        .get("profiles")
        .and_then(Value::as_object)
        .map(|profiles| profiles.values().all(|profile| decode_profile_adapter(profile.get("adapter")).is_some()))
        .unwrap_or(true);
    let models_ok = catalog
        .get("models")
        .and_then(Value::as_array)
        .map(|models| models.iter().all(|model| decode_model_adapter(model.get("adapter")).is_some()))
        .unwrap_or(true);
    profiles_ok && models_ok
}

/// `resolveProviderCatalog(manifest, "claudeAgent")` + the Claude adapters.
fn try_resolve(manifest: &Value) -> Option<ClaudeModelCatalog> {
    let catalog = manifest.get("providers")?.get("claudeAgent")?;
    let empty = Map::new();
    let profiles = catalog.get("profiles").and_then(Value::as_object).unwrap_or(&empty);
    let default_chat = catalog.get("defaults").and_then(|d| d.get("chat")).and_then(Value::as_str);
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    for entry in catalog.get("models").and_then(Value::as_array)? {
        let slug = entry.get("slug").and_then(Value::as_str)?;
        if !seen.insert(slug.to_string()) {
            return None;
        }
        let profile_name = entry.get("profile").and_then(Value::as_str);
        let profile = match profile_name {
            Some(name) => Some(profiles.get(name)?),
            None => None,
        };
        let mut model = Map::new();
        model.insert("slug".into(), Value::String(slug.to_string()));
        model.insert("name".into(), entry.get("name").cloned().unwrap_or(Value::Null));
        for key in ["shortName", "subProvider", "aliases", "badge"] {
            if let Some(value) = entry.get(key).filter(|v| !v.is_null() && v.as_str() != Some("")) {
                model.insert(key.into(), value.clone());
            }
        }
        model.insert("isCustom".into(), Value::Bool(false));
        if default_chat == Some(slug) {
            model.insert("isDefault".into(), Value::Bool(true));
        }
        if entry.get("status").and_then(Value::as_str) == Some("legacy") {
            model.insert("isLegacy".into(), Value::Bool(true));
        }
        model.insert(
            "capabilities".into(),
            profile.and_then(|p| p.get("capabilities")).cloned().unwrap_or(Value::Null),
        );
        let runtime = decode_profile_adapter(profile.and_then(|p| p.get("adapter")))?;
        let compatibility = decode_model_adapter(entry.get("adapter"))?;
        models.push(ClaudeCatalogModel {
            model: Value::Object(model),
            runtime,
            compatibility,
        });
    }
    if let Some(chat) = default_chat {
        if !seen.contains(chat) {
            return None;
        }
    }
    Some(ClaudeModelCatalog { models })
}

impl ClaudeModelCatalog {
    /// `resolveClaudeModelCatalog(manifest)`: the manifest's Claude models, else the bundled
    /// manifest's, else none.
    pub fn from_manifest(manifest: &Value) -> Self {
        try_resolve(manifest).unwrap_or_else(Self::bundled)
    }

    /// `BUNDLED_CLAUDE_MODEL_CATALOG`.
    pub fn bundled() -> Self {
        static BUNDLED: OnceLock<ClaudeModelCatalog> = OnceLock::new();
        BUNDLED
            .get_or_init(|| {
                serde_json::from_str::<Value>(BUNDLED_MODEL_MANIFEST_JSON)
                    .ok()
                    .and_then(|manifest| try_resolve(&manifest))
                    .unwrap_or_default()
            })
            .clone()
    }

    /// `scopeClaudeModelCatalog(catalog, customModels)`.
    pub fn scoped(&self, custom_models: &Value) -> Self {
        let custom_entries = read_custom_model_entries(custom_models);
        if custom_entries.is_empty() {
            return self.clone();
        }
        let custom_aliases: HashSet<String> = custom_entries.iter().map(|entry| entry.slug.to_lowercase()).collect();
        let built_in: Vec<ClaudeCatalogModel> = self
            .models
            .iter()
            .map(|entry| {
                if !entry.aliases().iter().any(|alias| custom_aliases.contains(&alias.to_lowercase())) {
                    return entry.clone();
                }
                let mut next = entry.clone();
                let kept: Vec<Value> = entry
                    .aliases()
                    .into_iter()
                    .filter(|alias| !custom_aliases.contains(&alias.to_lowercase()))
                    .map(|alias| Value::String(alias.to_string()))
                    .collect();
                if let Some(model) = next.model.as_object_mut() {
                    model.insert("aliases".into(), Value::Array(kept));
                }
                next
            })
            .collect();
        let built_in_slugs: HashSet<String> = built_in.iter().map(|entry| entry.slug().to_string()).collect();
        let mut models = built_in;
        for entry in custom_entries {
            let Some(capabilities) = entry.capabilities else { continue };
            if built_in_slugs.contains(&entry.slug) {
                continue;
            }
            models.push(ClaudeCatalogModel {
                model: serde_json::json!({ "slug": entry.slug, "name": entry.name, "isCustom": true, "capabilities": capabilities }),
                runtime: ClaudeCodeProfile::default(),
                compatibility: ClaudeCodeCompatibility::default(),
            });
        }
        Self { models }
    }

    /// `resolveClaudeCatalogModel`: by slug, then by alias (case-insensitive).
    pub fn resolve(&self, slug_or_alias: Option<&str>) -> Option<&ClaudeCatalogModel> {
        let value = slug_or_alias.map(str::trim).filter(|value| !value.is_empty())?;
        self.models.iter().find(|entry| entry.slug() == value).or_else(|| {
            self.models
                .iter()
                .find(|entry| entry.aliases().iter().any(|alias| alias.to_lowercase() == value.to_lowercase()))
        })
    }

    /// `resolveClaudeModelSlug`.
    pub fn resolve_slug(&self, slug_or_alias: &str) -> String {
        self.resolve(Some(slug_or_alias))
            .map(|entry| entry.slug().to_string())
            .unwrap_or_else(|| slug_or_alias.to_string())
    }

    /// `getClaudeCatalogModelCapabilities`.
    pub fn capabilities(&self, slug_or_alias: Option<&str>) -> Value {
        self.resolve(slug_or_alias)
            .map(ClaudeCatalogModel::capabilities)
            .unwrap_or_else(empty_capabilities)
    }

    /// `resolveClaudeModelsForVersion`: the `ServerProviderModel`s this CLI version can run.
    pub fn models_for_version(&self, version: Option<&str>) -> Vec<Value> {
        self.models
            .iter()
            .filter(|entry| is_version_supported(&entry.compatibility, version))
            .map(|entry| entry.model.clone())
            .collect()
    }

    /// `formatClaudeVersionUpgradeMessage`.
    pub fn version_upgrade_message(&self, version: Option<&str>) -> Option<String> {
        let mut candidates: Vec<&ClaudeCatalogModel> = self
            .models
            .iter()
            .filter(|entry| match &entry.compatibility.min_version {
                Some(min) => version.is_none_or(|v| compare_semver_versions(v, min) == Ordering::Less),
                None => false,
            })
            .collect();
        candidates.sort_by(|a, b| {
            compare_semver_versions(
                a.compatibility.min_version.as_deref().unwrap_or(""),
                b.compatibility.min_version.as_deref().unwrap_or(""),
            )
        });
        let unavailable = candidates.first()?;
        let min = unavailable.compatibility.min_version.as_deref()?;
        let label = version.map(|v| format!("v{v}")).unwrap_or_else(|| "the installed version".into());
        Some(format!(
            "Claude Code {label} is too old for {}. Upgrade to v{min} or newer to access it.",
            unavailable.name()
        ))
    }

    /// `resolveClaudeCatalogEffort(catalog, model, raw)`.
    pub fn resolve_effort(&self, model: Option<&str>, raw: Option<&str>) -> Option<String> {
        let caps = self.capabilities(model);
        let selections: Vec<Value> = raw
            .filter(|r| !r.is_empty())
            .map(|raw| vec![serde_json::json!({"id": "effort", "value": raw})])
            .unwrap_or_default();
        let descriptors = provider_option_descriptors(&caps, &selections);
        match provider_option_current_value(find_descriptor(&descriptors, "effort")) {
            Some(OptionValue::Str(value)) => Some(value),
            _ => None,
        }
    }

    /// `normalizeClaudeCatalogEffort(catalog, effort, model)`: the API effort for an option
    /// value (`ultracode` → `xhigh`, `ultrathink` → none).
    pub fn normalize_effort(&self, effort: Option<&str>, model: Option<&str>) -> Option<String> {
        let effort = effort.filter(|e| !e.is_empty())?;
        let Some(effort_map) = self.resolve(model).and_then(|entry| entry.runtime.effort_map.as_ref()) else {
            return Some(effort.to_string());
        };
        match effort_map.get(effort) {
            None => Some(effort.to_string()),
            Some(mapped) => mapped.clone(),
        }
    }

    fn context_window_option(&self, selection: Option<&ModelSelection>) -> Option<String> {
        let caps = self.capabilities(selection.map(|s| s.model.as_str()));
        let raw = selection_string_option(selection, "contextWindow");
        let selections: Vec<Value> = raw
            .filter(|r| !r.is_empty())
            .map(|raw| vec![serde_json::json!({"id": "contextWindow", "value": raw})])
            .unwrap_or_default();
        let descriptors = provider_option_descriptors(&caps, &selections);
        match provider_option_current_value(find_descriptor(&descriptors, "contextWindow")) {
            Some(OptionValue::Str(value)) => Some(value),
            _ => None,
        }
    }

    /// `resolveClaudeCatalogApiModelId`: the slug plus the suffix its options select.
    pub fn api_model_id(&self, selection: &ModelSelection) -> String {
        let entry = self.resolve(Some(&selection.model));
        let slug = entry.map(|e| e.slug().to_string()).unwrap_or_else(|| selection.model.to_string());
        let caps = entry.map(ClaudeCatalogModel::capabilities).unwrap_or_else(empty_capabilities);
        let descriptors = provider_option_descriptors(&caps, &selections_of(Some(selection)));
        if let Some(suffixes) = entry.and_then(|e| e.runtime.model_suffixes.as_ref()) {
            for (option_id, values) in suffixes {
                if let Some(OptionValue::Str(value)) = provider_option_current_value(find_descriptor(&descriptors, option_id)) {
                    if let Some(suffix) = values.get(&value).filter(|s| !s.is_empty()) {
                        return format!("{slug}{suffix}");
                    }
                }
            }
        }
        slug
    }

    /// `resolveClaudeCatalogContextWindowTokens`.
    pub fn context_window_tokens(&self, selection: Option<&ModelSelection>) -> Option<f64> {
        let entry = self.resolve(selection.map(|s| s.model.as_str()))?;
        if let Some(fixed) = entry.runtime.fixed_context_window_tokens.filter(|v| *v != 0.0) {
            return Some(fixed);
        }
        let option = self.context_window_option(selection)?;
        entry.runtime.context_window_tokens.as_ref()?.get(&option).copied()
    }
}

/// `isClaudeCatalogUltracodeEffort`.
pub fn is_ultracode_effort(effort: Option<&str>) -> bool {
    effort == Some("ultracode")
}

fn is_version_supported(compatibility: &ClaudeCodeCompatibility, version: Option<&str>) -> bool {
    if compatibility.min_version.is_none() && compatibility.max_version_exclusive.is_none() {
        return true;
    }
    let Some(version) = version.filter(|v| !v.is_empty()) else { return false };
    if let Some(min) = &compatibility.min_version {
        if compare_semver_versions(version, min) == Ordering::Less {
            return false;
        }
    }
    !matches!(&compatibility.max_version_exclusive, Some(max) if compare_semver_versions(version, max) != Ordering::Less)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use zc_contracts::ProviderInstanceId;

    fn manifest() -> Value {
        json!({
            "version": 1, "currentModels": {},
            "providers": {"claudeAgent": {
                "profiles": {"synthetic": {
                    "capabilities": {"optionDescriptors": [
                        {"id": "effort", "label": "Reasoning", "type": "select", "options": [{"id": "extreme", "label": "Extreme", "isDefault": true}]},
                        {"id": "contextWindow", "label": "Context Window", "type": "select", "options": [{"id": "large", "label": "Large", "isDefault": true}]}
                    ]},
                    "adapter": {"claudeCode": {"effortMap": {"extreme": "high"}, "modelSuffixes": {"contextWindow": {"large": "[large]"}}}}
                }},
                "models": [{"slug": "claude-synthetic-next", "name": "Claude Synthetic Next", "aliases": ["synthetic"], "status": "current", "profile": "synthetic", "adapter": {"claudeCode": {"minVersion": "3.2.0"}}}]
            }}
        })
    }

    fn selection(model: &str, options: Value) -> ModelSelection {
        serde_json::from_value(json!({"instanceId": "claudeAgent", "model": model, "options": options})).unwrap()
    }

    #[test]
    fn filters_models_at_runtime_version_boundaries_and_derives_the_upgrade_message() {
        let catalog = ClaudeModelCatalog::from_manifest(&manifest());
        assert!(catalog.models_for_version(Some("3.1.9")).is_empty());
        assert_eq!(
            catalog
                .models_for_version(Some("3.2.0"))
                .iter()
                .map(|m| m["slug"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["claude-synthetic-next"]
        );
        assert_eq!(
            catalog.version_upgrade_message(Some("3.1.9")).as_deref(),
            Some("Claude Code v3.1.9 is too old for Claude Synthetic Next. Upgrade to v3.2.0 or newer to access it.")
        );
    }

    #[test]
    fn resolves_aliases_and_declarative_adapter_mappings() {
        let mut input = manifest();
        let models = input["providers"]["claudeAgent"]["models"].as_array_mut().unwrap();
        models.insert(
            0,
            json!({"slug": "claude-synthetic-collision", "name": "Claude Synthetic Collision", "aliases": ["claude-synthetic-next"], "status": "current"}),
        );
        let catalog = ClaudeModelCatalog::from_manifest(&input);
        assert_eq!(catalog.resolve_slug("synthetic"), "claude-synthetic-next");
        assert_eq!(catalog.resolve_slug("claude-synthetic-next"), "claude-synthetic-next");
        assert_eq!(catalog.normalize_effort(Some("extreme"), Some("synthetic")).as_deref(), Some("high"));
        let selection = ModelSelection {
            instance_id: ProviderInstanceId::from("claudeAgent"),
            model: "synthetic".into(),
            options: None,
        };
        assert_eq!(catalog.api_model_id(&selection), "claude-synthetic-next[large]");
    }

    #[test]
    fn rejects_malformed_adapter_mappings() {
        let mut malformed = manifest();
        malformed["providers"]["claudeAgent"]["profiles"]["synthetic"]["adapter"] = json!({"claudeCode": {"effortMap": {"extreme": 123}}});
        assert!(!has_valid_claude_manifest_adapters(&malformed));
        assert!(has_valid_claude_manifest_adapters(&manifest()));
    }

    #[test]
    fn appends_custom_models_with_their_own_descriptors_and_keeps_bare_slugs_opaque() {
        let catalog = ClaudeModelCatalog::from_manifest(&manifest()).scoped(&json!([
            "synthetic",
            {"slug": "claude-custom-tuned", "name": "Tuned", "capabilities": {"optionDescriptors": [
                {"id": "effort", "label": "Reasoning", "type": "select", "options": [{"id": "gentle", "label": "Gentle", "isDefault": true}, {"id": "brutal", "label": "Brutal"}]}
            ]}}
        ]));
        assert_eq!(catalog.resolve_slug("synthetic"), "synthetic");
        assert_eq!(catalog.resolve_effort(Some("synthetic"), Some("extreme")), None);
        assert_eq!(catalog.resolve_effort(Some("claude-custom-tuned"), Some("brutal")).as_deref(), Some("brutal"));
        assert_eq!(catalog.resolve_effort(Some("claude-custom-tuned"), Some("bogus")).as_deref(), Some("gentle"));
        assert_eq!(catalog.normalize_effort(Some("brutal"), Some("claude-custom-tuned")).as_deref(), Some("brutal"));
        assert_eq!(
            catalog.api_model_id(&selection("claude-custom-tuned", json!([{"id": "effort", "value": "brutal"}]))),
            "claude-custom-tuned"
        );
        assert_eq!(
            catalog
                .models_for_version(Some("3.2.0"))
                .iter()
                .map(|m| m["slug"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["claude-synthetic-next", "claude-custom-tuned"]
        );
    }

    #[test]
    fn the_bundled_manifest_resolves() {
        let catalog = ClaudeModelCatalog::bundled();
        assert!(!catalog.models.is_empty());
    }
}
