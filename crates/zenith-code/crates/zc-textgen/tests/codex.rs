//! Port of `textGeneration/CodexTextGeneration.test.ts` over a recording fake `codex`.

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{json, Value};
use support::*;
use zc_contracts::CodexSettings;
use zc_ports::TextGeneration;
use zc_provider_codex::managed::CodexEffectiveRuntime;
use zc_textgen::codex::{CodexModel, CodexModelsSource, CodexRuntimeResolver};
use zc_textgen::{CodexBackend, CodexTextGeneration};

#[derive(Default)]
struct Setup {
    launch_args: Option<&'static str>,
    environment: Option<BTreeMap<String, String>>,
    models: Vec<&'static str>,
    managed_runtime: bool,
}

struct Harness {
    fake: FakeCli,
    attachments: tempfile::TempDir,
    generation: CodexTextGeneration,
}

fn harness(setup: Setup) -> Harness {
    let fake = FakeCli::new("codex");
    let attachments = tempfile::tempdir().unwrap();
    let mut config = json!({"binaryPath": fake.binary.to_string_lossy()});
    if let Some(launch_args) = setup.launch_args {
        config["launchArgs"] = json!(launch_args);
    }
    let config: CodexSettings = serde_json::from_value(config).unwrap();
    let environment = setup.environment.unwrap_or_else(|| fake.environment());
    let models: Vec<CodexModel> = setup
        .models
        .iter()
        .map(|slug| CodexModel {
            slug: (*slug).to_owned(),
            is_custom: false,
        })
        .collect();
    let source: CodexModelsSource = Arc::new(move || {
        let models = models.clone();
        Box::pin(async move { models })
    });
    let mut backend = CodexBackend::new(config.clone(), environment.clone(), attachments.path().to_path_buf()).with_models(source);
    if setup.managed_runtime {
        let resolver: CodexRuntimeResolver = Arc::new(move || {
            let runtime = CodexEffectiveRuntime {
                config: config.clone(),
                environment: environment.clone(),
                revision: "test".into(),
            };
            Box::pin(async move { Ok(runtime) })
        });
        backend = backend.with_runtime(resolver);
    }
    Harness {
        fake,
        attachments,
        generation: CodexTextGeneration::new(backend),
    }
}

fn default_selection() -> zc_ports::contracts::ModelSelection {
    selection("codex", "gpt-5.4-mini", None)
}

/// `originalArgs.includes(` ${arg} `)` of the TS fake.
fn args_include(record: &Record, needle: &str) -> bool {
    format!(" {} ", record.args()).contains(&format!(" {needle} "))
}

fn config_value(record: &Record, prefix: &str) -> Option<String> {
    record
        .argv
        .windows(2)
        .filter(|pair| pair[0] == "--config")
        .map(|pair| pair[1].clone())
        .find(|value| value.starts_with(prefix))
}

#[tokio::test]
async fn dispatches_the_qualified_live_model() {
    for selected in ["gpt-5.6-luna", "openai.gpt-5.6-luna"] {
        let h = harness(Setup {
            models: vec!["openai.gpt-5.6-luna"],
            ..Setup::default()
        });
        h.fake.output(&json!({"title": "Bedrock title"}).to_string());
        let result = h
            .generation
            .generate_thread_title(title_input("Describe this change", selection("codex", selected, None)))
            .await
            .unwrap();
        assert_eq!(result.title, "Bedrock title");
        let record = h.fake.only_record();
        assert!(args_include(&record, "--model openai.gpt-5.6-luna"));
        assert!(!args_include(&record, "--model gpt-5.6-luna"));
    }
}

#[tokio::test]
async fn generates_and_sanitizes_commit_messages_without_branch_by_default() {
    let h = harness(Setup::default());
    h.fake.output(
        &json!({
            "subject": "  Add important change to the system with too much detail and a trailing period.\nsecondary line",
            "body": "\n- added migration\n- updated tests\n"
        })
        .to_string(),
    );
    let generated = h.generation.generate_commit_message(commit_input(default_selection())).await.unwrap();
    assert!(generated.subject.encode_utf16().count() <= 72);
    assert!(!generated.subject.ends_with('.'));
    assert_eq!(generated.body, "- added migration\n- updated tests");
    assert_eq!(generated.branch, None);
    assert!(!h.fake.only_record().stdin.contains("branch must be a short semantic git branch fragment"));
}

#[tokio::test]
async fn forwards_codex_service_tier_and_non_default_reasoning_effort_into_codex_exec_config() {
    let h = harness(Setup::default());
    h.fake.output(&json!({"subject": "Add important change", "body": ""}).to_string());
    h.generation
        .generate_commit_message(commit_input(selection(
            "codex",
            "gpt-5.4",
            Some(json!([{"id": "reasoningEffort", "value": "xhigh"}, {"id": "serviceTier", "value": "priority"}])),
        )))
        .await
        .unwrap();
    let record = h.fake.only_record();
    assert_eq!(config_value(&record, "service_tier=").as_deref(), Some("service_tier=\"priority\""));
    assert_eq!(
        config_value(&record, "model_reasoning_effort=").as_deref(),
        Some("model_reasoning_effort=\"xhigh\"")
    );
    assert!(!record.stdin.contains("branch must be a short semantic git branch fragment"));
}

#[tokio::test]
async fn omits_a_persisted_service_tier_for_managed_chatgpt_text_generation() {
    let h = harness(Setup {
        managed_runtime: true,
        ..Setup::default()
    });
    h.fake.output(&json!({"subject": "Update project", "body": ""}).to_string());
    h.generation
        .generate_commit_message(commit_input(selection(
            "codex",
            "gpt-5.4",
            Some(json!([{"id": "serviceTier", "value": "priority"}])),
        )))
        .await
        .unwrap();
    assert!(!args_include(&h.fake.only_record(), "service_tier=\"priority\""));
}

#[tokio::test]
async fn passes_exec_safe_launch_args_into_codex_exec() {
    let h = harness(Setup {
        launch_args: Some("--strict-config --listen off"),
        ..Setup::default()
    });
    h.fake.output(&json!({"subject": "Add important change", "body": ""}).to_string());
    h.generation.generate_commit_message(commit_input(default_selection())).await.unwrap();
    let record = h.fake.only_record();
    assert!(args_include(&record, "--strict-config"));
    assert!(!args_include(&record, "--listen"));
}

#[tokio::test]
async fn uses_t3code_codex_launch_args_for_codex_exec_over_settings() {
    let fake_env = FakeCli::new("codex");
    let mut environment = fake_env.environment();
    environment.insert("T3CODE_CODEX_LAUNCH_ARGS".into(), " --strict-config --listen off ".into());
    let h = harness(Setup {
        launch_args: Some("--enable settings-feature"),
        environment: Some(environment),
        ..Setup::default()
    });
    // The environment points FAKE_CLI_DIR at the other fake's data directory.
    fake_env.output(&json!({"subject": "Add important change", "body": ""}).to_string());
    h.generation.generate_commit_message(commit_input(default_selection())).await.unwrap();
    let record = fake_env.only_record();
    assert!(args_include(&record, "--strict-config"));
    assert!(!args_include(&record, "settings-feature"));
}

#[tokio::test]
async fn defaults_git_text_generation_codex_effort_to_low() {
    let h = harness(Setup::default());
    h.fake.output(&json!({"subject": "Add important change", "body": ""}).to_string());
    h.generation.generate_commit_message(commit_input(default_selection())).await.unwrap();
    assert_eq!(
        config_value(&h.fake.only_record(), "model_reasoning_effort=").as_deref(),
        Some("model_reasoning_effort=\"low\"")
    );
}

#[tokio::test]
async fn generates_commit_message_with_branch_when_include_branch_is_true() {
    let h = harness(Setup::default());
    h.fake
        .output(&json!({"subject": "Add important change", "body": "", "branch": "fix/important-system-change"}).to_string());
    let mut input = commit_input(default_selection());
    input.include_branch = true;
    let generated = h.generation.generate_commit_message(input).await.unwrap();
    assert_eq!(generated.subject, "Add important change");
    assert_eq!(generated.branch.as_deref(), Some("feature/fix/important-system-change"));
    let record = h.fake.only_record();
    assert!(record.stdin.contains("branch must be a short semantic git branch fragment"));
    let schema: Value = serde_json::from_str(record.schema.as_deref().unwrap()).unwrap();
    assert_eq!(schema["required"], json!(["subject", "body", "branch"]));
}

#[tokio::test]
async fn generates_pr_content_and_trims_markdown_body() {
    let h = harness(Setup::default());
    h.fake.output(
        &json!({"title": "  Improve orchestration flow\nwith ignored suffix", "body": "\n## Summary\n- improve flow\n\n## Testing\n- bun test\n\n"})
            .to_string(),
    );
    let generated = h.generation.generate_pr_content(pr_input(default_selection())).await.unwrap();
    assert_eq!(generated.title, "Improve orchestration flow");
    assert!(generated.body.starts_with("## Summary"));
    assert!(!generated.body.ends_with("\n\n"));
}

#[tokio::test]
async fn generates_branch_names_and_normalizes_branch_fragments() {
    let h = harness(Setup::default());
    h.fake.output(&json!({"branch": "  Feat/Session  "}).to_string());
    let branch = h
        .generation
        .generate_branch_name(branch_input("Please update session handling.", default_selection()))
        .await
        .unwrap();
    assert_eq!(branch, "feat/session");
    assert!(!h.fake.only_record().stdin.contains("Image attachments supplied to the model"));
}

#[tokio::test]
async fn generates_thread_titles_and_trims_them_for_sidebar_use() {
    let h = harness(Setup::default());
    h.fake
        .output(&json!({"title": "  \"Investigate websocket reconnect regressions after worktree restore\"  \nignored line"}).to_string());
    let generated = h
        .generation
        .generate_thread_title(title_input(
            "Please investigate websocket reconnect regressions after a worktree restore.",
            default_selection(),
        ))
        .await
        .unwrap();
    assert_eq!(generated.title, "Investigate websocket reconnect regressions after worktree restore");
}

#[tokio::test]
async fn returns_the_refinement_signal_for_an_unresolved_subject() {
    let h = harness(Setup::default());
    h.fake.output(&json!({"title": "Investigate issue", "needsRefinement": true}).to_string());
    let generated = h.generation.generate_thread_title(title_input("Fix this", default_selection())).await.unwrap();
    assert_eq!(generated.title, "Investigate issue");
    assert_eq!(generated.needs_refinement, Some(true));
}

#[tokio::test]
async fn falls_back_when_thread_title_normalization_becomes_whitespace_only() {
    let h = harness(Setup::default());
    h.fake.output(&json!({"title": "  \"\"\"   \"\"\"  "}).to_string());
    let generated = h
        .generation
        .generate_thread_title(title_input("Name this thread.", default_selection()))
        .await
        .unwrap();
    assert_eq!(generated.title, "New thread");
}

#[tokio::test]
async fn trims_whitespace_exposed_after_quote_removal_in_thread_titles() {
    let h = harness(Setup::default());
    h.fake.output(&json!({"title": "  \"' hello world '\"  "}).to_string());
    let generated = h
        .generation
        .generate_thread_title(title_input("Name this thread.", default_selection()))
        .await
        .unwrap();
    assert_eq!(generated.title, "hello world");
}

#[tokio::test]
async fn omits_attachment_metadata_section_when_no_attachments_are_provided() {
    let h = harness(Setup::default());
    h.fake.output(&json!({"branch": "fix/session-timeout"}).to_string());
    let branch = h
        .generation
        .generate_branch_name(branch_input("Fix timeout behavior.", default_selection()))
        .await
        .unwrap();
    assert_eq!(branch, "fix/session-timeout");
    assert!(!h.fake.only_record().stdin.contains("Attachment metadata:"));
}

#[tokio::test]
async fn passes_image_attachments_through_as_codex_image_inputs() {
    let h = harness(Setup::default());
    h.fake.output(&json!({"branch": "fix/ui-regression"}).to_string());
    let image = h.attachments.path().join("thread-branch-image-attachment.png");
    std::fs::write(&image, b"hello").unwrap();
    let mut input = branch_input("Fix layout bug from screenshot.", default_selection());
    input.attachments = vec![image_attachment("thread-branch-image-attachment", "bug.png")];
    let branch = h.generation.generate_branch_name(input).await.unwrap();
    assert_eq!(branch, "fix/ui-regression");
    let record = h.fake.only_record();
    let image_arg = record.value_after("--image").expect("missing --image input");
    assert_eq!(std::fs::canonicalize(image_arg).unwrap(), std::fs::canonicalize(&image).unwrap());
    assert!(record.stdin.contains("Attachment metadata:"));
    assert!(image.exists(), "the persisted attachment stays");
}

#[tokio::test]
async fn ignores_missing_attachment_ids_for_codex_image_inputs() {
    let h = harness(Setup::default());
    h.fake.output(&json!({"branch": "fix/ui-regression"}).to_string());
    let mut input = branch_input("Fix layout bug from screenshot.", default_selection());
    input.attachments = vec![image_attachment("thread-missing-attachment", "outside.png")];
    h.generation.generate_branch_name(input).await.unwrap();
    // The TS fake fails with "missing --image input" here: no image reached the CLI.
    assert!(!h.fake.only_record().has("--image"));
}

#[tokio::test]
async fn fails_with_typed_text_generation_error_when_codex_returns_wrong_branch_payload_shape() {
    let h = harness(Setup::default());
    h.fake.output(&json!({"title": "This is not a branch payload"}).to_string());
    let error = h
        .generation
        .generate_branch_name(branch_input("Fix websocket reconnect flake", default_selection()))
        .await
        .unwrap_err();
    assert_eq!(error.tag, "TextGenerationError");
    assert!(error.message.contains("Codex returned invalid structured output"));
}

#[tokio::test]
async fn returns_typed_text_generation_error_when_codex_exits_non_zero() {
    let h = harness(Setup::default());
    h.fake.output(&json!({"subject": "ignored", "body": ""}).to_string());
    h.fake.stderr("codex execution failed").exit(1);
    let error = h.generation.generate_commit_message(commit_input(default_selection())).await.unwrap_err();
    assert_eq!(error.tag, "TextGenerationError");
    assert!(error.message.contains("Codex CLI command failed: codex execution failed"));
}

// Rust additions.

#[tokio::test]
async fn removes_its_temp_files_and_passes_the_schema() {
    let h = harness(Setup::default());
    h.fake.output(&json!({"title": "Made-up title"}).to_string());
    h.generation.generate_thread_title(title_input("Name it", default_selection())).await.unwrap();
    let record = h.fake.only_record();
    let schema_path = record.value_after("--output-schema").unwrap().to_owned();
    let output_path = record.value_after("--output-last-message").unwrap().to_owned();
    assert!(schema_path.contains(&format!("t3code-codex-schema-{}-", std::process::id())));
    assert!(output_path.contains(&format!("t3code-codex-output-{}-", std::process::id())));
    assert!(!std::path::Path::new(&schema_path).exists());
    assert!(!std::path::Path::new(&output_path).exists());
    assert_eq!(
        record.schema.as_deref(),
        Some(
            r#"{"type":"object","properties":{"title":{"type":"string"},"needsRefinement":{"type":"boolean"}},"required":["title","needsRefinement"],"additionalProperties":false}"#
        )
    );
    assert_eq!(record.argv.last().map(String::as_str), Some("-"));
}

#[tokio::test]
async fn managed_runtime_failures_carry_the_setup_detail() {
    let fake = FakeCli::new("codex");
    let config: CodexSettings = serde_json::from_value(json!({"binaryPath": fake.binary.to_string_lossy()})).unwrap();
    let resolver: CodexRuntimeResolver = Arc::new(|| {
        Box::pin(async {
            Err(serde_json::from_value(
                json!({"_tag": "ProviderSetupError", "instanceId": "codex", "operation": "install", "detail": "Set up managed Codex before starting a session."}),
            )
            .unwrap())
        })
    });
    let generation = CodexTextGeneration::new(CodexBackend::new(config, fake.environment(), fake.root.path().to_path_buf()).with_runtime(resolver));
    let error = generation.generate_commit_message(commit_input(default_selection())).await.unwrap_err();
    assert_eq!(detail(&error), "Set up managed Codex before starting a session.");
    assert!(fake.records().is_empty());
}
