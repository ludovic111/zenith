//! Model helpers the Codex driver uses (`@t3tools/shared/model`, `codexModelOptions.ts`, the
//! Codex constants of `contracts/model.ts`).

use serde_json::Value;
use zc_contracts::{CustomModelSetting, ModelCapabilities, ModelSelection, ProviderOptionSelection, ProviderOptionSelectionValue, ProviderOptionSelections};

/// `DEFAULT_MODEL` (the Codex default model).
pub const DEFAULT_MODEL: &str = "gpt-6-astra";

/// `PREFERRED_DEFAULT_CODEX_MODELS`, most preferred first.
pub const PREFERRED_DEFAULT_CODEX_MODELS: &[&str] = &[DEFAULT_MODEL, "gpt-5.6-sol", "gpt-5.6-terra"];

/// `MODEL_SLUG_ALIASES_BY_PROVIDER.codex`.
const CODEX_MODEL_SLUG_ALIASES: &[(&str, &str)] = &[
    ("gpt-5-codex", "gpt-5.4"),
    ("5.4", "gpt-5.4"),
    ("5.3", "gpt-5.3-codex"),
    ("gpt-5.3", "gpt-5.3-codex"),
    ("5.3-spark", "gpt-5.3-codex-spark"),
    ("gpt-5.3-spark", "gpt-5.3-codex-spark"),
];

/// `normalizeCustomModelSlug`: trimmed, `None` when blank.
pub fn normalize_custom_model_slug(model: Option<&str>) -> Option<String> {
    model.map(str::trim).filter(|model| !model.is_empty()).map(str::to_owned)
}

/// `normalizeModelSlug(model, "codex")`: trimmed, Codex aliases expanded.
pub fn normalize_model_slug(model: Option<&str>) -> Option<String> {
    let trimmed = normalize_custom_model_slug(model)?;
    Some(
        CODEX_MODEL_SLUG_ALIASES
            .iter()
            .find(|(alias, _)| *alias == trimmed)
            .map_or(trimmed, |(_, target)| (*target).to_owned()),
    )
}

/// `codexModelFamily`: `openai.gpt-*` dispatch ids compare as `gpt-*`.
pub fn codex_model_family(slug: &str) -> &str {
    if slug.starts_with("openai.gpt-") {
        &slug["openai.".len()..]
    } else {
        slug
    }
}

fn selections(selection: Option<&ModelSelection>) -> &[ProviderOptionSelection] {
    match selection.and_then(|selection| selection.options.as_ref()) {
        Some(ProviderOptionSelections::Array(values) | ProviderOptionSelections::Array_(values)) => values,
        None => &[],
    }
}

fn raw_selection<'a>(selection: Option<&'a ModelSelection>, id: &str) -> Option<&'a ProviderOptionSelectionValue> {
    selections(selection)
        .iter()
        .find(|candidate| candidate.id == id)
        .map(|candidate| &candidate.value)
}

/// `getModelSelectionStringOptionValue`.
pub fn model_selection_string_option(selection: Option<&ModelSelection>, id: &str) -> Option<String> {
    match raw_selection(selection, id)? {
        ProviderOptionSelectionValue::TrimmedNonEmptyString(value) => Some(value.clone()),
        ProviderOptionSelectionValue::Bool(_) => None,
    }
}

/// `getModelSelectionBooleanOptionValue`.
pub fn model_selection_bool_option(selection: Option<&ModelSelection>, id: &str) -> Option<bool> {
    match raw_selection(selection, id)? {
        ProviderOptionSelectionValue::Bool(value) => Some(*value),
        ProviderOptionSelectionValue::TrimmedNonEmptyString(_) => None,
    }
}

/// `getCodexServiceTierOptionValue`: the `serviceTier` option, or `fast` for a legacy
/// `fastMode: true`.
pub fn codex_service_tier_option_value(selection: Option<&ModelSelection>) -> Option<String> {
    model_selection_string_option(selection, "serviceTier")
        .or_else(|| (model_selection_bool_option(selection, "fastMode") == Some(true)).then(|| "fast".to_owned()))
}

/// `CustomModelDefinition`.
#[derive(Debug, Clone, PartialEq)]
pub struct CustomModelDefinition {
    pub slug: String,
    pub name: String,
    pub capabilities: Option<ModelCapabilities>,
}

/// `readCustomModelEntries`: trimmed, deduplicated (first wins), name falls back to the slug,
/// capabilities kept only when they decode.
pub fn read_custom_model_entries(value: &[CustomModelSetting]) -> Vec<CustomModelDefinition> {
    let mut entries: Vec<CustomModelDefinition> = Vec::new();
    for raw in value {
        let (slug, name, capabilities) = match raw {
            CustomModelSetting::String(slug) => (Some(slug.as_str()), None, None),
            CustomModelSetting::CustomModelEntry(entry) => (Some(entry.slug.as_str()), entry.name.as_deref(), entry.capabilities.clone()),
        };
        let Some(slug) = normalize_custom_model_slug(slug) else { continue };
        if entries.iter().any(|entry| entry.slug == slug) {
            continue;
        }
        let name = normalize_custom_model_slug(name).unwrap_or_else(|| slug.clone());
        let capabilities = capabilities.map(|capabilities| ModelCapabilities {
            option_descriptors: Some(capabilities.option_descriptors.unwrap_or_default()),
        });
        entries.push(CustomModelDefinition { slug, name, capabilities });
    }
    entries
}

/// Builds a contract value from JSON written in the TS shape.
pub(crate) fn from_json<T: serde::de::DeserializeOwned>(value: Value) -> T {
    match serde_json::from_value(value.clone()) {
        Ok(decoded) => decoded,
        Err(error) => panic!("a value built in the contract shape must decode ({error}): {value}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection(options: Value) -> ModelSelection {
        from_json(serde_json::json!({"instanceId": "codex", "model": "gpt-5.5", "options": options}))
    }

    #[test]
    fn service_tier_option_value() {
        assert_eq!(
            codex_service_tier_option_value(Some(&selection(serde_json::json!([{"id": "serviceTier", "value": "flex"}])))).as_deref(),
            Some("flex")
        );
        assert_eq!(
            codex_service_tier_option_value(Some(&selection(serde_json::json!([{"id": "fastMode", "value": true}])))).as_deref(),
            Some("fast")
        );
        assert_eq!(
            codex_service_tier_option_value(Some(&selection(serde_json::json!([{"id": "fastMode", "value": false}])))),
            None
        );
        assert_eq!(codex_service_tier_option_value(None), None);
    }

    #[test]
    fn slugs_and_families() {
        assert_eq!(normalize_model_slug(Some(" 5.3 ")).as_deref(), Some("gpt-5.3-codex"));
        assert_eq!(normalize_model_slug(Some("gpt-5.6-sol")).as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(normalize_model_slug(Some("  ")), None);
        assert_eq!(codex_model_family("openai.gpt-6-astra"), "gpt-6-astra");
        assert_eq!(codex_model_family("gpt-6-astra"), "gpt-6-astra");
    }

    #[test]
    fn custom_model_entries() {
        let settings: Vec<CustomModelSetting> =
            from_json(serde_json::json!([" a ", {"slug": "a"}, {"slug": "b", "name": " Bee "}, {"slug": "c", "capabilities": {}}]));
        let entries = read_custom_model_entries(&settings);
        assert_eq!(entries.iter().map(|entry| entry.slug.as_str()).collect::<Vec<_>>(), vec!["a", "b", "c"]);
        assert_eq!(entries[1].name, "Bee");
        assert_eq!(
            entries[2].capabilities,
            Some(ModelCapabilities {
                option_descriptors: Some(vec![])
            })
        );
    }
}
