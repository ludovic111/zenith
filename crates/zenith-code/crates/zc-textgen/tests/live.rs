//! Live smoke tests against the real CLIs (they spend the account's quota, so they are
//! ignored by default). One thread title per CLI, for a made-up message, in a temp directory:
//!
//! ```sh
//! ZC_TEXTGEN_LIVE_CLAUDE=$(command -v claude) cargo test -p zc-textgen --test live -- --ignored live_claude
//! ZC_TEXTGEN_LIVE_CODEX=/path/to/codex cargo test -p zc-textgen --test live -- --ignored live_codex
//! ```

use serde_json::json;
use zc_ports::contracts::ModelSelection;
use zc_ports::text_generation::ThreadTitleGenerationInput;
use zc_ports::TextGeneration;
use zc_provider_claude::home::process_env;
use zc_provider_claude::ClaudeModelCatalog;
use zc_textgen::claude::fixed_catalog;
use zc_textgen::{ClaudeBackend, CodexBackend, OneShotTextGeneration};

const MESSAGE: &str = "The moonbeam project sidebar flickers whenever I switch between two made-up workspaces; please find out why and fix it.";

fn input(cwd: &std::path::Path, instance: &str, model: &str) -> ThreadTitleGenerationInput {
    ThreadTitleGenerationInput {
        linked_context: None,
        cwd: cwd.to_string_lossy().into_owned(),
        message: MESSAGE.into(),
        previous_title: None,
        attachments: Vec::new(),
        model_selection: ModelSelection(json!({"instanceId": instance, "model": model})),
    }
}

#[tokio::test]
#[ignore = "spends the Claude account's quota"]
async fn live_claude_title() {
    let Ok(binary) = std::env::var("ZC_TEXTGEN_LIVE_CLAUDE") else {
        eprintln!("set ZC_TEXTGEN_LIVE_CLAUDE to the claude binary");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let settings = serde_json::from_value(json!({"binaryPath": binary})).unwrap();
    let generation = OneShotTextGeneration::new(ClaudeBackend::new(settings, process_env(), fixed_catalog(ClaudeModelCatalog::bundled())));
    let title = generation
        .generate_thread_title(input(dir.path(), "claudeAgent", "claude-haiku-4-5"))
        .await
        .unwrap();
    println!("claude title: {title:?}");
    assert!(!title.title.is_empty() && title.title != "New thread");
}

#[tokio::test]
#[ignore = "spends the Codex account's quota"]
async fn live_codex_title() {
    let Ok(binary) = std::env::var("ZC_TEXTGEN_LIVE_CODEX") else {
        eprintln!("set ZC_TEXTGEN_LIVE_CODEX to the codex binary");
        return;
    };
    let model = std::env::var("ZC_TEXTGEN_LIVE_CODEX_MODEL").unwrap_or_else(|_| "gpt-6-astra".into());
    let dir = tempfile::tempdir().unwrap();
    let settings = serde_json::from_value(json!({"binaryPath": binary})).unwrap();
    let generation = OneShotTextGeneration::new(CodexBackend::new(settings, process_env(), dir.path().join("attachments")));
    let title = generation.generate_thread_title(input(dir.path(), "codex", &model)).await.unwrap();
    println!("codex title: {title:?}");
    assert!(!title.title.is_empty() && title.title != "New thread");
}
