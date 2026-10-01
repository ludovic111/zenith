//! The model-option helpers of `packages/shared/src/model.ts` the Claude driver uses
//! (`getProviderOptionDescriptors`, `getProviderOptionCurrentValue`, the selection readers,
//! `resolvePromptInjectedEffort`, `applyClaudePromptEffortPrefix`, `readCustomModelEntries`).
//!
//! Descriptors stay JSON (`ModelCapabilities.optionDescriptors` as the manifest and the settings
//! carry them) so every field the client renders passes through untouched.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Map, Value};
use zc_contracts::ModelSelection;

/// One option value: a string choice or a boolean toggle.
#[derive(Debug, Clone, PartialEq)]
pub enum OptionValue {
    Str(String),
    Bool(bool),
}

impl OptionValue {
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::String(s) => Some(Self::Str(s.clone())),
            Value::Bool(b) => Some(Self::Bool(*b)),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            Self::Bool(_) => None,
        }
    }
}

/// The `{id, value}` selections of a model selection, as JSON.
pub fn selections_of(selection: Option<&ModelSelection>) -> Vec<Value> {
    selection
        .and_then(|selection| selection.options.as_ref())
        .and_then(|options| serde_json::to_value(options).ok())
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
}

fn raw_selection_value(selections: &[Value], id: &str) -> Option<OptionValue> {
    selections
        .iter()
        .find(|candidate| candidate.get("id").and_then(Value::as_str) == Some(id))
        .and_then(|selection| selection.get("value"))
        .and_then(OptionValue::from_value)
}

/// `getModelSelectionStringOptionValue`.
pub fn selection_string_option(selection: Option<&ModelSelection>, id: &str) -> Option<String> {
    match raw_selection_value(&selections_of(selection), id) {
        Some(OptionValue::Str(value)) => Some(value),
        _ => None,
    }
}

/// `getModelSelectionBooleanOptionValue`.
pub fn selection_bool_option(selection: Option<&ModelSelection>, id: &str) -> Option<bool> {
    match raw_selection_value(&selections_of(selection), id) {
        Some(OptionValue::Bool(value)) => Some(value),
        _ => None,
    }
}

fn trim_or_none(value: Option<&str>) -> Option<String> {
    value.map(str::trim).filter(|value| !value.is_empty()).map(str::to_string)
}

fn descriptor_options(descriptor: &Value) -> Vec<Value> {
    descriptor.get("options").and_then(Value::as_array).cloned().unwrap_or_default()
}

fn default_option_id(descriptor: &Value) -> Option<String> {
    descriptor_options(descriptor)
        .iter()
        .find(|option| option.get("isDefault").and_then(Value::as_bool) == Some(true))
        .and_then(|option| option.get("id").and_then(Value::as_str))
        .map(str::to_string)
}

fn has_option(descriptor: &Value, id: &str) -> bool {
    descriptor_options(descriptor)
        .iter()
        .any(|option| option.get("id").and_then(Value::as_str) == Some(id))
}

fn prompt_injected_values(descriptor: &Value) -> Vec<String> {
    descriptor
        .get("promptInjectedValues")
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

fn descriptor_current_string(descriptor: &Value) -> Option<String> {
    descriptor.get("currentValue").and_then(Value::as_str).map(str::to_string)
}

/// `resolveDescriptorChoiceValue`.
fn resolve_descriptor_choice_value(descriptor: &Value, raw: Option<&str>) -> Option<String> {
    let Some(trimmed) = trim_or_none(raw) else {
        return descriptor_current_string(descriptor).or_else(|| default_option_id(descriptor));
    };
    if descriptor_options(descriptor).is_empty() {
        return Some(trimmed);
    }
    if prompt_injected_values(descriptor).contains(&trimmed) && has_option(descriptor, &trimmed) {
        return default_option_id(descriptor);
    }
    if has_option(descriptor, &trimmed) {
        return Some(trimmed);
    }
    descriptor_current_string(descriptor).or_else(|| default_option_id(descriptor))
}

/// `withDescriptorCurrentValue`.
fn with_descriptor_current_value(descriptor: &Value, raw: Option<OptionValue>) -> Value {
    let mut next = descriptor.clone();
    let is_boolean = descriptor.get("type").and_then(Value::as_str) == Some("boolean");
    let Some(object) = next.as_object_mut() else { return next };
    if is_boolean {
        if let Some(OptionValue::Bool(value)) = raw {
            object.insert("currentValue".into(), Value::Bool(value));
        }
        return next;
    }
    let current = match raw {
        Some(OptionValue::Str(value)) => resolve_descriptor_choice_value(descriptor, Some(&value)),
        _ => resolve_descriptor_choice_value(descriptor, descriptor.get("currentValue").and_then(Value::as_str)),
    };
    match current {
        Some(value) => {
            object.insert("currentValue".into(), Value::String(value));
        }
        None => {
            object.remove("currentValue");
        }
    }
    next
}

/// `getProviderOptionDescriptors({caps, selections})`.
pub fn provider_option_descriptors(capabilities: &Value, selections: &[Value]) -> Vec<Value> {
    capabilities
        .get("optionDescriptors")
        .and_then(Value::as_array)
        .map(|descriptors| {
            descriptors
                .iter()
                .map(|descriptor| {
                    let id = descriptor.get("id").and_then(Value::as_str).unwrap_or_default();
                    let raw = raw_selection_value(selections, id).or_else(|| descriptor.get("currentValue").and_then(OptionValue::from_value));
                    with_descriptor_current_value(descriptor, raw)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `getProviderOptionCurrentValue`.
pub fn provider_option_current_value(descriptor: Option<&Value>) -> Option<OptionValue> {
    let descriptor = descriptor?;
    if descriptor.get("type").and_then(Value::as_str) == Some("boolean") {
        return descriptor.get("currentValue").and_then(Value::as_bool).map(OptionValue::Bool);
    }
    if let Some(current) = descriptor.get("currentValue").and_then(Value::as_str).filter(|value| !value.is_empty()) {
        return Some(OptionValue::Str(current.to_string()));
    }
    default_option_id(descriptor).map(OptionValue::Str)
}

/// The descriptor with this id.
pub fn find_descriptor<'a>(descriptors: &'a [Value], id: &str) -> Option<&'a Value> {
    descriptors.iter().find(|descriptor| descriptor.get("id").and_then(Value::as_str) == Some(id))
}

/// `resolvePromptInjectedEffort(caps, rawEffort)`.
pub fn resolve_prompt_injected_effort(capabilities: &Value, raw_effort: Option<&str>) -> Option<String> {
    let trimmed = trim_or_none(raw_effort)?;
    provider_option_descriptors(capabilities, &[])
        .iter()
        .any(|descriptor| descriptor.get("type").and_then(Value::as_str) == Some("select") && prompt_injected_values(descriptor).contains(&trimmed))
        .then_some(trimmed)
}

fn slash_command_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `/^\/[^\s/]+(?:\s|$)/u` with JS `\s`.
    RE.get_or_init(|| {
        let ws = r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";
        Regex::new(&format!(r"^/[^{ws}/]+(?:[{ws}]|$)")).expect("valid regex")
    })
}

/// `applyClaudePromptEffortPrefix(text, effort)`.
pub fn apply_claude_prompt_effort_prefix(text: &str, effort: Option<&str>) -> String {
    let trimmed = text.trim_matches(crate::cli_args::is_js_space);
    if trimmed.is_empty() {
        return String::new();
    }
    if effort != Some("ultrathink") || slash_command_regex().is_match(trimmed) || trimmed.starts_with("Ultrathink:") {
        return trimmed.to_string();
    }
    format!("Ultrathink:\n{trimmed}")
}

/// A resolved custom model (`readCustomModelEntries`).
#[derive(Debug, Clone, PartialEq)]
pub struct CustomModelDefinition {
    pub slug: String,
    pub name: String,
    /// `ModelCapabilities` JSON, or `None` for a bare slug.
    pub capabilities: Option<Value>,
}

fn is_valid_capabilities(value: &Value) -> bool {
    // `Schema.decodeUnknownOption(ModelCapabilities)`: an object whose `optionDescriptors` is an
    // array of select/boolean descriptors with an id, a label and (for selects) options.
    let Some(descriptors) = value.get("optionDescriptors").and_then(Value::as_array) else {
        return value.is_object() && value.get("optionDescriptors").is_none();
    };
    descriptors.iter().all(|descriptor| {
        let id = descriptor.get("id").and_then(Value::as_str).is_some();
        let label = descriptor.get("label").and_then(Value::as_str).is_some();
        match descriptor.get("type").and_then(Value::as_str) {
            Some("boolean") => id && label,
            Some("select") => {
                id && label
                    && descriptor.get("options").and_then(Value::as_array).is_some_and(|options| {
                        options
                            .iter()
                            .all(|option| option.get("id").and_then(Value::as_str).is_some() && option.get("label").and_then(Value::as_str).is_some())
                    })
            }
            _ => false,
        }
    })
}

/// `readCustomModelEntries(value)`.
pub fn read_custom_model_entries(value: &Value) -> Vec<CustomModelDefinition> {
    let Some(raw_entries) = value.as_array() else { return Vec::new() };
    let mut entries = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for raw in raw_entries {
        let record = match raw {
            Value::String(slug) => json!({ "slug": slug }),
            Value::Object(_) => raw.clone(),
            _ => continue,
        };
        let Some(slug) = trim_or_none(record.get("slug").and_then(Value::as_str)) else {
            continue;
        };
        if !seen.insert(slug.clone()) {
            continue;
        }
        let name = trim_or_none(record.get("name").and_then(Value::as_str)).unwrap_or_else(|| slug.clone());
        let capabilities = match record.get("capabilities") {
            None | Some(Value::Null) => None,
            Some(capabilities) if is_valid_capabilities(capabilities) => {
                let descriptors = capabilities.get("optionDescriptors").cloned().unwrap_or_else(|| Value::Array(Vec::new()));
                Some(Value::Object(Map::from_iter([("optionDescriptors".to_string(), descriptors)])))
            }
            Some(_) => None,
        };
        entries.push(CustomModelDefinition { slug, name, capabilities });
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn effort_caps() -> Value {
        json!({"optionDescriptors": [{
            "id": "effort", "label": "Reasoning", "type": "select",
            "options": [{"id": "low", "label": "Low"}, {"id": "high", "label": "High", "isDefault": true}, {"id": "ultrathink", "label": "U"}],
            "promptInjectedValues": ["ultrathink"]
        }]})
    }

    #[test]
    fn resolves_descriptor_current_values() {
        let caps = effort_caps();
        let descriptors = provider_option_descriptors(&caps, &[json!({"id": "effort", "value": "low"})]);
        assert_eq!(
            provider_option_current_value(find_descriptor(&descriptors, "effort")),
            Some(OptionValue::Str("low".into()))
        );
        let descriptors = provider_option_descriptors(&caps, &[json!({"id": "effort", "value": "bogus"})]);
        assert_eq!(
            provider_option_current_value(find_descriptor(&descriptors, "effort")),
            Some(OptionValue::Str("high".into()))
        );
        let descriptors = provider_option_descriptors(&caps, &[json!({"id": "effort", "value": "ultrathink"})]);
        assert_eq!(
            provider_option_current_value(find_descriptor(&descriptors, "effort")),
            Some(OptionValue::Str("high".into()))
        );
        assert_eq!(resolve_prompt_injected_effort(&caps, Some("ultrathink")), Some("ultrathink".into()));
        assert_eq!(resolve_prompt_injected_effort(&caps, Some("low")), None);
    }

    #[test]
    fn prefixes_ultrathink_except_for_slash_commands() {
        assert_eq!(apply_claude_prompt_effort_prefix(" fix it ", Some("ultrathink")), "Ultrathink:\nfix it");
        assert_eq!(apply_claude_prompt_effort_prefix("/compact", Some("ultrathink")), "/compact");
        assert_eq!(
            apply_claude_prompt_effort_prefix("/home/user/app.ts is broken", Some("ultrathink")),
            "Ultrathink:\n/home/user/app.ts is broken"
        );
        assert_eq!(apply_claude_prompt_effort_prefix("x", Some("high")), "x");
    }

    #[test]
    fn reads_custom_model_entries() {
        let entries =
            read_custom_model_entries(&json!(["a", " a ", {"slug": "b", "name": " "}, {"slug": "c", "capabilities": {"optionDescriptors": "bad"}}, 3]));
        assert_eq!(entries.iter().map(|e| e.slug.as_str()).collect::<Vec<_>>(), vec!["a", "b", "c"]);
        assert_eq!(entries[1].name, "b");
        assert_eq!(entries[2].capabilities, None);
    }
}
