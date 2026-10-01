//! Port of `textGeneration/ClaudeTextGeneration.test.ts` over a recording fake `claude`.

mod support;

use serde_json::{json, Value};
use support::*;
use zc_contracts::ClaudeSettings;
use zc_ports::TextGeneration;
use zc_textgen::claude::fixed_catalog;
use zc_textgen::utils::sanitize_thread_title;
use zc_textgen::{ClaudeBackend, ClaudeTextGeneration};

fn settings(fake: &FakeCli, extra: Value) -> ClaudeSettings {
    let mut config = json!({"binaryPath": fake.binary.to_string_lossy()});
    config.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    serde_json::from_value(config).unwrap()
}

fn generator(fake: &FakeCli, extra: Value) -> ClaudeTextGeneration {
    ClaudeTextGeneration::new(ClaudeBackend::new(
        settings(fake, extra),
        fake.environment(),
        fixed_catalog(synthetic_catalog()),
    ))
}

/// The invariants the TS fake enforces on every call (exit codes 6–12 there).
fn assert_locked_down(record: &Record) {
    assert_eq!(
        record.value_after("--permission-mode"),
        Some("dontAsk"),
        "text generation must deny permission prompts"
    );
    assert_eq!(
        record.value_after("--tools"),
        Some(""),
        "text generation must receive an explicit empty tool set"
    );
    assert!(!record.has("--dangerously-skip-permissions"), "text generation must not bypass permissions");
    assert!(record.has("--disable-slash-commands"), "text generation must disable skills");
    assert!(record.has("--strict-mcp-config"), "text generation must not load configured MCP servers");
    let settings: Value = serde_json::from_str(record.value_after("--settings").unwrap()).unwrap();
    assert_eq!(settings["disableAllHooks"], json!(true), "text generation must disable hooks");
}

fn claude_output(structured: Value) -> String {
    json!({"structured_output": structured}).to_string()
}

#[tokio::test]
async fn forwards_claude_thinking_settings_without_passing_unsupported_effort() {
    let fake = FakeCli::new("claude");
    fake.stdout(&claude_output(json!({"subject": "Add important change", "body": ""})));
    let generated = generator(&fake, json!({}))
        .generate_commit_message(commit_input(selection(
            "claudeAgent",
            THINKING,
            Some(json!([{"id": "thinking", "value": false}, {"id": "effort", "value": "high"}])),
        )))
        .await
        .unwrap();
    assert_eq!(generated.subject, "Add important change");
    let record = fake.only_record();
    assert_locked_down(&record);
    assert!(record.args().contains(r#"--settings {"disableAllHooks":true,"alwaysThinkingEnabled":false}"#));
    assert!(!record.args().contains("--effort"));
}

#[tokio::test]
async fn keeps_a_configured_custom_alias_opaque_to_the_claude_cli() {
    let fake = FakeCli::new("claude");
    fake.stdout(&claude_output(json!({"title": "Keep custom model", "body": ""})));
    let generated = generator(&fake, json!({"customModels": [COLLIDING_ALIAS]}))
        .generate_pr_content(pr_input(selection(
            "claudeAgent",
            COLLIDING_ALIAS,
            Some(json!([{"id": "effort", "value": "max"}, {"id": "fastMode", "value": true}, {"id": "contextWindow", "value": "expanded"}])),
        )))
        .await
        .unwrap();
    assert_eq!(generated.title, "Keep custom model");
    let record = fake.only_record();
    assert_locked_down(&record);
    assert!(record.args().contains(&format!("--model {COLLIDING_ALIAS} --settings")));
}

#[tokio::test]
async fn keeps_canonical_built_in_capabilities_when_a_custom_model_collides_with_its_alias() {
    let fake = FakeCli::new("claude");
    fake.stdout(&claude_output(json!({"title": "Improve orchestration flow", "body": "Body"})));
    let generated = generator(&fake, json!({"customModels": [COLLIDING_ALIAS]}))
        .generate_pr_content(pr_input(selection(
            "claudeAgent",
            CAPABLE,
            Some(json!([{"id": "effort", "value": "max"}, {"id": "fastMode", "value": true}])),
        )))
        .await
        .unwrap();
    assert_eq!(generated.title, "Improve orchestration flow");
    assert!(fake.only_record().args().contains(&format!(
        r#"--model {CAPABLE}[expanded] --effort max --settings {{"disableAllHooks":true,"fastMode":true}}"#
    )));
}

#[tokio::test]
async fn generates_thread_titles_outside_the_project_with_tools_skills_and_hooks_disabled() {
    let fake = FakeCli::new("claude");
    fake.stdout(&claude_output(
        json!({"title": "  \"Reconnect failures after restart because the session state does not recover\"  "}),
    ));
    let generated = generator(&fake, json!({}))
        .generate_thread_title(title_input("/call-script", selection("claudeAgent", STANDARD, None)))
        .await
        .unwrap();
    assert_eq!(
        generated.title,
        sanitize_thread_title("\"Reconnect failures after restart because the session state does not recover\"")
    );
    let record = fake.only_record();
    assert_locked_down(&record);
    let project = std::fs::canonicalize(project_cwd()).unwrap();
    assert_ne!(std::path::PathBuf::from(&record.cwd), project, "text generation ran in the project directory");
    assert!(record.cwd.contains("t3code-claude-title-"));
    assert!(!std::path::Path::new(&record.cwd).exists(), "the title directory is removed afterwards");
    assert!(record.stdin.contains("/call-script"));
}

#[tokio::test]
async fn generates_branch_names_from_skill_prompts_without_executable_capabilities() {
    let fake = FakeCli::new("claude");
    fake.stdout(&claude_output(json!({"branch": "call-script"})));
    let branch = generator(&fake, json!({}))
        .generate_branch_name(branch_input("/call-script", selection("claudeAgent", STANDARD, None)))
        .await
        .unwrap();
    assert_eq!(branch, "call-script");
    let record = fake.only_record();
    assert_locked_down(&record);
    assert!(record.stdin.contains("/call-script"));
    assert_eq!(std::path::PathBuf::from(&record.cwd), std::fs::canonicalize(project_cwd()).unwrap());
}

#[tokio::test]
async fn runs_claude_text_generation_with_the_configured_claude_config_dir() {
    let fake = FakeCli::new("claude");
    let config_dir = fake.root.path().join(".claude-work-test").to_string_lossy().into_owned();
    fake.stdout(&claude_output(json!({"title": "Use Claude home"})));
    let generated = generator(&fake, json!({"homePath": config_dir}))
        .generate_thread_title(title_input("thread title", selection("claudeAgent", STANDARD, None)))
        .await
        .unwrap();
    assert_eq!(generated.title, sanitize_thread_title("Use Claude home"));
    assert_eq!(fake.only_record().env.get("CLAUDE_CONFIG_DIR"), Some(&config_dir));
}

#[tokio::test]
async fn unwraps_a_json_title_in_normal_and_verbose_claude_output() {
    for verbose in [false, true] {
        let fake = FakeCli::new("claude");
        let result = json!({"type": "result", "structured_output": {"title": "{\"title\": \"Refresh made-up APP ASG instances\"}"}});
        fake.stdout(&if verbose { json!([result]) } else { result }.to_string());
        let generated = generator(&fake, json!({}))
            .generate_thread_title(title_input("Refresh made-up APP ASG instances", selection("claudeAgent", STANDARD, None)))
            .await
            .unwrap();
        assert_eq!(generated.title, "Refresh made-up APP ASG instances", "verbose: {verbose}");
    }
}

#[tokio::test]
async fn reads_the_result_from_verbose_claude_output_when_generating_or_regenerating_a_title() {
    for previous_title in [None, Some("Old thread title")] {
        let fake = FakeCli::new("claude");
        fake.stdout(
            &json!([
                {"type": "system", "subtype": "init"},
                {"type": "assistant", "message": {"content": []}},
                {"type": "user", "message": {"content": []}},
                {"type": "rate_limit_event"},
                {"type": "result", "subtype": "success", "result": "{\"title\":\"Refresh Made-up APP ASG Instances\"}",
                 "structured_output": {"title": "Refresh Made-up APP ASG Instances"}}
            ])
            .to_string(),
        );
        let mut input = title_input("Refresh made-up APP ASG instances", selection("claudeAgent", STANDARD, None));
        input.previous_title = previous_title.map(str::to_owned);
        let generated = generator(&fake, json!({})).generate_thread_title(input).await.unwrap();
        assert_eq!(generated.title, "Refresh Made-up APP ASG Instances");
    }
}

#[tokio::test]
async fn rejects_verbose_claude_output_without_a_usable_result() {
    let cases = [
        ("empty message array", json!([])),
        ("missing result", json!([{"type": "assistant", "structured_output": {"title": "Not a result"}}])),
        ("invalid title", json!([{"type": "result", "structured_output": {"title": 42}}])),
        (
            "final result without structured output",
            json!([{"type": "result", "structured_output": {"title": "Earlier result"}}, {"type": "result", "subtype": "error_max_structured_output_retries"}]),
        ),
    ];
    for (name, output) in cases {
        let fake = FakeCli::new("claude");
        fake.stdout(&output.to_string());
        let error = generator(&fake, json!({}))
            .generate_thread_title(title_input("Name this thread", selection("claudeAgent", STANDARD, None)))
            .await
            .unwrap_err();
        assert_eq!(error.tag, "TextGenerationError", "{name}");
        assert_eq!(detail(&error), "Claude returned invalid structured output.", "{name}");
    }
}

#[tokio::test]
async fn falls_back_when_claude_thread_title_normalization_becomes_whitespace_only() {
    let fake = FakeCli::new("claude");
    fake.stdout(&claude_output(json!({"title": "  \"\"\"   \"\"\"  "})));
    let generated = generator(&fake, json!({}))
        .generate_thread_title(title_input("Name this thread.", selection("claudeAgent", STANDARD, None)))
        .await
        .unwrap();
    assert_eq!(generated.title, "New thread");
}

// Rust additions: the failure paths `runClaudeJson` maps.

#[tokio::test]
async fn reports_cli_failures_with_their_output() {
    let fake = FakeCli::new("claude");
    fake.stderr("made-up failure").exit(3);
    let error = generator(&fake, json!({}))
        .generate_branch_name(branch_input("x", selection("claudeAgent", STANDARD, None)))
        .await
        .unwrap_err();
    assert_eq!(detail(&error), "Claude CLI command failed: made-up failure");
    assert_eq!(
        error.message,
        "Text generation failed in generateBranchName: Claude CLI command failed: made-up failure"
    );

    let fake = FakeCli::new("claude");
    fake.exit(4);
    let error = generator(&fake, json!({}))
        .generate_branch_name(branch_input("x", selection("claudeAgent", STANDARD, None)))
        .await
        .unwrap_err();
    assert_eq!(detail(&error), "Claude CLI command failed with code 4.");

    let fake = FakeCli::new("claude");
    fake.stdout("not json");
    let error = generator(&fake, json!({}))
        .generate_branch_name(branch_input("x", selection("claudeAgent", STANDARD, None)))
        .await
        .unwrap_err();
    assert_eq!(detail(&error), "Claude CLI returned unexpected output format.");
}

#[tokio::test]
async fn names_a_missing_claude_binary_and_times_out() {
    let fake = FakeCli::new("claude");
    let mut config = settings(&fake, json!({}));
    config.binary_path = fake.root.path().join("missing-claude").to_string_lossy().into_owned();
    let missing = ClaudeTextGeneration::new(ClaudeBackend::new(config, fake.environment(), fixed_catalog(synthetic_catalog())));
    let error = missing
        .generate_branch_name(branch_input("x", selection("claudeAgent", STANDARD, None)))
        .await
        .unwrap_err();
    assert_eq!(detail(&error), "Claude CLI (`claude`) is required but not available on PATH.");

    let slow = FakeCli::new("claude");
    std::fs::write(&slow.binary, "#!/bin/sh\nsleep 5\n").unwrap();
    let generation = ClaudeTextGeneration::new(
        ClaudeBackend::new(settings(&slow, json!({})), slow.environment(), fixed_catalog(synthetic_catalog()))
            .with_timeout(std::time::Duration::from_millis(200)),
    );
    let error = generation
        .generate_branch_name(branch_input("x", selection("claudeAgent", STANDARD, None)))
        .await
        .unwrap_err();
    assert_eq!(detail(&error), "Claude CLI request timed out.");
}
