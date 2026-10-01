//! Turn instructions (`provider/CodexDeveloperInstructions.ts`, `provider/RuntimeInstructions.ts`):
//! the collaboration-mode prompt and the `additionalContext` entries of `turn/start`.

use std::collections::BTreeMap;

use zc_codex_protocol::{AdditionalContextEntry, AdditionalContextKind};
use zc_contracts::ProviderInteractionMode;

use crate::instruction_texts::{
    CODEX_DEFAULT_MODE_DEVELOPER_INSTRUCTIONS, CODEX_PLAN_MODE_DEVELOPER_INSTRUCTIONS, PULL_REQUEST_LINKING_INSTRUCTIONS, T3_CODE_BROWSER_TOOL_INSTRUCTIONS,
    T3_CODE_DEVICE_TOOL_INSTRUCTIONS,
};

/// Which `t3-code` MCP toolkits a turn has (`T3CodeToolAvailability`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToolAvailability {
    pub browser: bool,
    pub device: bool,
}

impl ToolAvailability {
    /// `true` / `false` in TS means the browser toolkit only.
    pub fn browser_only(browser: bool) -> Self {
        Self { browser, device: false }
    }
}

/// `CodexRuntimeInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CodexRuntimeInfo {
    pub model: String,
    pub model_name: Option<String>,
    pub reasoning_effort: String,
}

/// `buildCodexDeveloperInstructions`: the mode prompt for
/// `turn/start.collaborationMode.settings.developer_instructions`.
pub fn build_codex_developer_instructions(mode: ProviderInteractionMode) -> &'static str {
    match mode {
        ProviderInteractionMode::Plan => CODEX_PLAN_MODE_DEVELOPER_INSTRUCTIONS,
        ProviderInteractionMode::Default => CODEX_DEFAULT_MODE_DEVELOPER_INSTRUCTIONS,
    }
}

fn tool_instructions(tools: ToolAvailability) -> String {
    [
        if tools.browser { T3_CODE_BROWSER_TOOL_INSTRUCTIONS } else { "" },
        if tools.device { T3_CODE_DEVICE_TOOL_INSTRUCTIONS } else { "" },
    ]
    .into_iter()
    .filter(|block| !block.is_empty())
    .collect::<Vec<_>>()
    .join("\n\n")
}

/// `buildCodexAdditionalContext`: `t3_code_runtime`, plus `t3_code_tools` when any toolkit is
/// attached (separate keys keep each value under Codex's per-entry token cap).
pub fn build_codex_additional_context(runtime: &CodexRuntimeInfo, tools: ToolAvailability) -> BTreeMap<String, AdditionalContextEntry> {
    let mut context = BTreeMap::new();
    context.insert(
        "t3_code_runtime".to_owned(),
        AdditionalContextEntry {
            kind: AdditionalContextKind::Application,
            value: build_runtime_instructions("Codex", Some(&runtime.model), runtime.model_name.as_deref(), Some(&runtime.reasoning_effort)),
        },
    );
    let tools = tool_instructions(tools);
    if !tools.is_empty() {
        context.insert(
            "t3_code_tools".to_owned(),
            AdditionalContextEntry {
                kind: AdditionalContextKind::Application,
                value: tools,
            },
        );
    }
    context
}

fn to_single_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `buildRuntimeInstructions`: the shared runtime context.
pub fn build_runtime_instructions(harness: &str, model: Option<&str>, model_name: Option<&str>, reasoning_effort: Option<&str>) -> String {
    let harness = to_single_line(harness);
    let model = to_single_line(model.unwrap_or_default());
    let model_name = to_single_line(model_name.unwrap_or_default());
    let effort = to_single_line(reasoning_effort.unwrap_or_default());
    let model_label = if !model_name.is_empty() && model_name != model {
        format!("{model_name} (model slug: {model})")
    } else {
        model.clone()
    };
    let model_info = if !model.is_empty() && model != "auto" && model != "default" {
        format!(", as {model_label}")
    } else {
        String::new()
    };
    let effort_info = if effort.is_empty() {
        String::new()
    } else {
        format!(" with {effort} reasoning effort")
    };
    format!(
        "<runtime_info>In case you're asked: you are running in {} through the {harness} harness{model_info}{effort_info}. No need to mention this otherwise. You can embed images and videos in your response using Markdown with absolute file paths.</runtime_info>\n\n{PULL_REQUEST_LINKING_INSTRUCTIONS}",
        crate::BRAND_NAME
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime(model: &str, effort: &str) -> CodexRuntimeInfo {
        CodexRuntimeInfo {
            model: model.into(),
            model_name: None,
            reasoning_effort: effort.into(),
        }
    }

    #[test]
    fn mode_prompts_stay_free_of_runtime_context() {
        for mode in [ProviderInteractionMode::Default, ProviderInteractionMode::Plan] {
            let text = build_codex_developer_instructions(mode);
            assert!(text.starts_with("<collaboration_mode>") && text.ends_with("</collaboration_mode>"));
            for forbidden in ["runtime_info", "pull_request_linking", "preview_", "device_"] {
                assert!(!text.contains(forbidden));
            }
        }
    }

    #[test]
    fn runtime_context_describes_harness_model_and_effort() {
        let context = build_codex_additional_context(&runtime("gpt-5.3-codex", "high"), ToolAvailability::browser_only(true));
        let value = &context["t3_code_runtime"].value;
        assert_eq!(context["t3_code_runtime"].kind, AdditionalContextKind::Application);
        assert!(value.starts_with("<runtime_info>"));
        assert!(value.contains("Codex harness, as gpt-5.3-codex with high reasoning effort"));
        assert!(value.contains("embed images and videos"));
    }

    #[test]
    fn runtime_context_varies_with_model_and_effort() {
        let a = build_codex_additional_context(&runtime("gpt-5.3-codex", "medium"), ToolAvailability::default());
        let b = build_codex_additional_context(&runtime("gpt-5.4", "high"), ToolAvailability::default());
        assert_ne!(a["t3_code_runtime"].value, b["t3_code_runtime"].value);
    }

    #[test]
    fn multiline_metadata_is_flattened() {
        let context = build_codex_additional_context(&runtime("gpt\n5.3\ncodex", " high\neffort "), ToolAvailability::default());
        let value = &context["t3_code_runtime"].value;
        assert!(value.contains("as gpt 5.3 codex with high effort reasoning effort"));
        let info = value.split("</runtime_info>").next().unwrap();
        assert!(!info.contains('\n'));
    }

    #[test]
    fn entries_stay_under_the_token_cap() {
        let context = build_codex_additional_context(&runtime("gpt-5.3-codex", "high"), ToolAvailability { browser: true, device: true });
        for entry in context.values() {
            assert!(entry.value.len() < 4_000);
        }
    }

    #[test]
    fn tool_blocks_follow_the_granted_toolkits() {
        let browser = build_codex_additional_context(&runtime("m", "high"), ToolAvailability::browser_only(true));
        let tools = &browser["t3_code_tools"].value;
        assert!(tools.contains("t3-code") && tools.contains("preview_status") && tools.contains("preview_open"));
        assert!(tools.contains("Do not switch to global browser skills"));
        assert!(!tools.contains("device_open"));

        let device = build_codex_additional_context(&runtime("m", "high"), ToolAvailability { browser: false, device: true });
        assert!(device["t3_code_tools"].value.contains("device_open"));
        assert!(!device["t3_code_tools"].value.contains("preview_open"));

        let none = build_codex_additional_context(&runtime("m", "high"), ToolAvailability::browser_only(false));
        assert_eq!(none.keys().collect::<Vec<_>>(), vec!["t3_code_runtime"]);
    }

    #[test]
    fn model_display_name_and_slug() {
        let value = build_runtime_instructions("Codex", Some("gpt-5.3-codex"), Some("GPT-5.3-Codex"), Some("high"));
        assert!(value.contains("as GPT-5.3-Codex (model slug: gpt-5.3-codex) with high reasoning effort"));
        let auto = build_runtime_instructions("Codex", Some("auto"), None, None);
        assert!(auto.contains("through the Codex harness. No need"));
    }
}
