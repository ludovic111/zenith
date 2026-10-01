//! Golden comparison against the TypeScript server: the same calls run through the TS
//! `ClaudeTextGeneration` / `CodexTextGeneration` (`golden/textgen_oracle.mjs`, from source,
//! with the real effect/contracts packages) and through the Rust generators, each over its own
//! copy of the recording fake CLI. For every call, the CLI's argv, environment, working
//! directory, stdin and `--output-schema` file must be identical, and so must the results and
//! errors.
//!
//! Normalized: the fakes' own directories, the random part of temp paths (their parent, the
//! temp dir, must match), the process id in Codex temp file names, and the build's branding of
//! the prompt (the TS source says "T3 Code"; the built server, like Rust, says "zenith").
//!
//! Needs `node` and `code/apps/server/node_modules` (symlinked from a checkout that has them);
//! without them the test prints why and passes vacuously.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use regex::Regex;
use serde_json::{json, Value};
use support::*;
use zc_ports::contracts::{ChatAttachment, ModelSelection};
use zc_ports::text_generation::{BranchNameGenerationInput, CommitMessageGenerationInput, PrContentGenerationInput, ThreadTitleGenerationInput};
use zc_ports::{TaggedError, TextGeneration};
use zc_textgen::claude::fixed_catalog;
use zc_textgen::codex::{CodexModel, CodexModelsSource};
use zc_textgen::prompts::rebrand;
use zc_textgen::{ClaudeBackend, CodexBackend, OneShotTextGeneration};

fn server_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../code/apps/server")
}

fn oracle_available() -> Result<(), String> {
    if !server_dir().join("node_modules/effect").exists() {
        return Err(format!("{} has no node_modules", server_dir().display()));
    }
    match Command::new("node").arg("--version").output() {
        Ok(output) if output.status.success() => Ok(()),
        _ => Err("node is not installed".into()),
    }
}

fn run_oracle(request: &Value) -> Value {
    let script = include_str!("golden/textgen_oracle.mjs");
    let server = std::fs::canonicalize(server_dir()).unwrap();
    let mut child = Command::new("node")
        .args(["--no-warnings", "--input-type=module", "-e", script])
        .current_dir(&server)
        .env("ZC_SERVER_SRC", server.join("src"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(request.to_string().as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "oracle failed: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json = stdout.rsplit("@@ORACLE@@").next().unwrap_or_default();
    serde_json::from_str(json).unwrap_or_else(|e| panic!("oracle output: {e}: {stdout}"))
}

/// Write the fake's answers for one call (what the oracle's `setFake` does).
fn set_fake(fake: &FakeCli, answers: &Value) {
    for name in ["stdout", "stderr", "output", "exit"] {
        let file = fake.data.join(name);
        let _ = std::fs::remove_file(&file);
        if let Some(value) = answers.get(name) {
            let text = value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string());
            std::fs::write(&file, text).unwrap();
        }
    }
}

fn string(input: &Value, key: &str) -> String {
    input[key].as_str().unwrap_or_default().to_owned()
}

fn optional(input: &Value, key: &str) -> Option<String> {
    input.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn attachments(input: &Value) -> Vec<ChatAttachment> {
    input
        .get("attachments")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(ChatAttachment)
        .collect()
}

fn error_json(error: TaggedError) -> Value {
    json!({"error": {"_tag": error.tag, "operation": error.fields["operation"], "detail": error.fields["detail"]}})
}

async fn call_rust(generation: &dyn TextGeneration, op: &str, input: &Value) -> Value {
    let model_selection = ModelSelection(input["modelSelection"].clone());
    let policy = input.get("policy").map(|policy| serde_json::from_value(policy.clone()).unwrap());
    let result = match op {
        "generateCommitMessage" => generation
            .generate_commit_message(CommitMessageGenerationInput {
                cwd: string(input, "cwd"),
                branch: optional(input, "branch"),
                staged_summary: string(input, "stagedSummary"),
                staged_patch: string(input, "stagedPatch"),
                include_branch: input["includeBranch"].as_bool().unwrap_or(false),
                policy,
                model_selection,
            })
            .await
            .map(|result| {
                let mut value = json!({"subject": result.subject, "body": result.body});
                if let Some(branch) = result.branch {
                    value["branch"] = json!(branch);
                }
                value
            }),
        "generatePrContent" => generation
            .generate_pr_content(PrContentGenerationInput {
                cwd: string(input, "cwd"),
                base_branch: string(input, "baseBranch"),
                head_branch: string(input, "headBranch"),
                commit_summary: string(input, "commitSummary"),
                diff_summary: string(input, "diffSummary"),
                diff_patch: string(input, "diffPatch"),
                change_request_template: optional(input, "changeRequestTemplate"),
                policy,
                model_selection,
            })
            .await
            .map(|result| json!({"title": result.title, "body": result.body})),
        "generateBranchName" => generation
            .generate_branch_name(BranchNameGenerationInput {
                cwd: string(input, "cwd"),
                message: string(input, "message"),
                attachments: attachments(input),
                model_selection,
            })
            .await
            .map(|branch| json!({"branch": branch})),
        "generateThreadTitle" => generation
            .generate_thread_title(ThreadTitleGenerationInput {
                linked_context: optional(input, "linkedContext"),
                cwd: string(input, "cwd"),
                message: string(input, "message"),
                previous_title: optional(input, "previousTitle"),
                attachments: attachments(input),
                model_selection,
            })
            .await
            .map(|result| {
                let mut value = json!({"title": result.title});
                if let Some(flag) = result.needs_refinement {
                    value["needsRefinement"] = json!(flag);
                }
                value
            }),
        other => panic!("unknown op {other}"),
    };
    match result {
        Ok(value) => json!({"ok": value}),
        Err(error) => error_json(error),
    }
}

/// A record with everything that legitimately differs between the two runs replaced.
fn normalize(record: &Record, fake: &FakeCli, ts: bool) -> Record {
    let data = fake.data.to_string_lossy().into_owned();
    let data_real = std::fs::canonicalize(&fake.data).unwrap().to_string_lossy().into_owned();
    let temp = Regex::new(r"(t3code-(?:claude-title|codex-schema-\d+|codex-output-\d+)-)[^/\s]+").unwrap();
    let pid = Regex::new(r"t3code-codex-(schema|output)-\d+-").unwrap();
    let hex_file = Regex::new(r"<random>/[0-9a-f]{12}$").unwrap();
    let clean = |text: &str| -> String {
        let text = text.replace(&data_real, "<fake>").replace(&data, "<fake>");
        let text = temp.replace_all(&text, "${1}<random>").into_owned();
        let text = hex_file.replace_all(&text, "<random>/<hex12>").into_owned();
        pid.replace_all(&text, "t3code-codex-$1-<pid>-").into_owned()
    };
    Record {
        argv: record.argv.iter().map(|arg| clean(arg)).collect(),
        env: record.env.iter().map(|(key, value)| (key.clone(), clean(value))).collect(),
        cwd: clean(&record.cwd),
        stdin: if ts { rebrand(&record.stdin) } else { record.stdin.clone() },
        schema: record.schema.clone(),
    }
}

struct Run {
    provider: &'static str,
    config: Value,
    extra_env: Vec<(&'static str, &'static str)>,
    models: Vec<&'static str>,
    calls: Vec<Value>,
}

async fn compare(run: Run) {
    let base = tempfile::Builder::new().prefix("zc-textgen-golden-").tempdir().unwrap();
    let base_dir = std::fs::canonicalize(base.path()).unwrap();
    let cwd = base_dir.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd = cwd.to_string_lossy().into_owned();
    let binary_name = if run.provider == "claude" { "claude" } else { "codex" };
    let ts_fake = FakeCli::new(binary_name);
    let rust_fake = FakeCli::new(binary_name);
    let config_for = |fake: &FakeCli| {
        let mut config = run.config.clone();
        config["binaryPath"] = json!(fake.binary.to_string_lossy());
        if let Some(home) = config.get("homePath").and_then(Value::as_str) {
            config["homePath"] = json!(base_dir.join(home).to_string_lossy());
        }
        config
    };
    let env_for = |fake: &FakeCli| {
        let mut env = fake.environment();
        for (key, value) in &run.extra_env {
            env.insert((*key).to_owned(), (*value).to_owned());
        }
        env
    };
    let calls: Vec<Value> = run
        .calls
        .iter()
        .map(|call| {
            let mut call = call.clone();
            call["input"]["cwd"] = json!(cwd);
            call
        })
        .collect();

    let oracle = run_oracle(&json!({
        "provider": run.provider,
        "config": config_for(&ts_fake),
        "environment": env_for(&ts_fake),
        "baseDir": base_dir.join("server").to_string_lossy(),
        "models": run.models,
        "calls": calls,
    }));
    let attachments_dir = PathBuf::from(oracle["attachmentsDir"].as_str().unwrap());

    let generation: Arc<dyn TextGeneration> = if run.provider == "claude" {
        let settings = serde_json::from_value(config_for(&rust_fake)).unwrap();
        Arc::new(OneShotTextGeneration::new(ClaudeBackend::new(
            settings,
            env_for(&rust_fake),
            fixed_catalog(synthetic_catalog()),
        )))
    } else {
        let settings = serde_json::from_value(config_for(&rust_fake)).unwrap();
        let models: Vec<CodexModel> = run
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
        Arc::new(OneShotTextGeneration::new(
            CodexBackend::new(settings, env_for(&rust_fake), attachments_dir.clone()).with_models(source),
        ))
    };
    let mut rust_results = Vec::new();
    for call in &calls {
        set_fake(&rust_fake, &call["fake"]);
        rust_results.push(call_rust(generation.as_ref(), call["op"].as_str().unwrap(), &call["input"]).await);
    }

    let ts_records = ts_fake.records();
    let rust_records = rust_fake.records();
    assert_eq!(ts_records.len(), rust_records.len(), "{} CLI calls", run.provider);
    assert!(!ts_records.is_empty());
    for (index, (ts, rust)) in ts_records.iter().zip(&rust_records).enumerate() {
        let ts = normalize(ts, &ts_fake, true);
        let rust = normalize(rust, &rust_fake, false);
        assert_eq!(ts.argv, rust.argv, "{} call {index}: argv", run.provider);
        assert_eq!(ts.env, rust.env, "{} call {index}: env", run.provider);
        assert_eq!(ts.cwd, rust.cwd, "{} call {index}: cwd", run.provider);
        assert_eq!(ts.stdin, rust.stdin, "{} call {index}: stdin", run.provider);
        assert_eq!(ts.schema, rust.schema, "{} call {index}: schema", run.provider);
    }
    let ts_results = oracle["results"].as_array().unwrap();
    for (index, (ts, rust)) in ts_results.iter().zip(&rust_results).enumerate() {
        assert_eq!(ts, rust, "{} call {index}: result", run.provider);
    }
    assert_eq!(ts_results.len(), rust_results.len());
    // The scenario reaches the provider-specific paths it is meant to cover.
    if run.provider == "codex" {
        assert!(rust_records.iter().any(|record| record.has("--image")), "an image attachment reached codex");
    } else {
        assert!(
            rust_records.iter().any(|record| record.cwd.contains("t3code-claude-title-")),
            "titles ran outside the project"
        );
        assert!(rust_records.iter().any(|record| record.has("--effort")), "an effort reached claude");
    }
    println!(
        "{}: {} calls, {} CLI invocations identical",
        run.provider,
        rust_results.len(),
        rust_records.len()
    );
}

fn all_fields() -> Value {
    json!({"subject": "Add made-up change.", "body": "\n- one\n", "branch": "Made Up/Branch", "title": "  \"Made-up title\"  ", "needsRefinement": false})
}

fn sel(instance: &str, model: &str, options: Value) -> Value {
    if options.is_null() {
        json!({"instanceId": instance, "model": model})
    } else {
        json!({"instanceId": instance, "model": model, "options": options})
    }
}

fn image(id: &str, name: &str) -> Value {
    json!({"type": "image", "id": id, "name": name, "mimeType": "image/png", "sizeBytes": 5})
}

fn calls(instance: &str, models: [&str; 3], options: [Value; 3], answer: impl Fn(Value) -> Value) -> Vec<Value> {
    let ok = answer(all_fields());
    vec![
        json!({"op": "generateCommitMessage", "fake": ok, "input": {
            "branch": "feature/made-up", "stagedSummary": "M README.md", "stagedPatch": "diff --git a/README.md b/README.md\n+hello",
            "includeBranch": true, "policy": {"kind": "custom", "commitInstructions": "  Use a terse made-up style.  ", "inferRepositoryConventions": false},
            "modelSelection": sel(instance, models[0], options[0].clone())}}),
        json!({"op": "generateCommitMessage", "fake": ok, "input": {
            "branch": null, "stagedSummary": "M a.ts", "stagedPatch": "x".repeat(41_000),
            "modelSelection": sel(instance, models[1], options[1].clone())}}),
        json!({"op": "generatePrContent", "fake": ok, "input": {
            "baseBranch": "main", "headBranch": "feature/made-up", "commitSummary": "feat: made up", "diffSummary": "1 file changed",
            "diffPatch": "diff", "changeRequestTemplate": "<!-- drop -->\n## What\n\n## Why",
            "policy": {"kind": "conventional_commits", "changeRequestInstructions": "Keep it short.", "inferRepositoryConventions": false},
            "modelSelection": sel(instance, models[2], options[2].clone())}}),
        json!({"op": "generateBranchName", "fake": answer(json!({"branch": "  Feat/Made Up  "})),
            "attachmentFiles": [{"name": "made-up-image.png", "content": "hello"}],
            "input": {"message": "Fix the made-up layout", "attachments": [image("made-up-image", "shot.png"), image("missing-image", "gone.png")],
                      "modelSelection": sel(instance, models[0], Value::Null)}}),
        json!({"op": "generateThreadTitle", "fake": answer(json!({"title": "{\"title\": \"Made-up JSON title\"}", "needsRefinement": true})),
            "input": {"message": "Review https://forge.test/change/1 please", "linkedContext": "https://forge.test/change/1\n{\"title\":\"Made up\",\"body\":\"\"}",
                      "attachments": [image("made-up-image", "shot.png")], "modelSelection": sel(instance, models[1], options[1].clone())}}),
        json!({"op": "generateThreadTitle", "fake": ok, "input": {
            "message": format!("[Earlier content truncated]\n\n{}", "USER:\nmade up\n\n".repeat(900)), "previousTitle": "Old \"quoted\" title",
            "modelSelection": sel(instance, models[2], Value::Null)}}),
        json!({"op": "generateCommitMessage", "fake": {"stderr": "made-up failure", "exit": 3}, "input": {
            "branch": "main", "stagedSummary": "", "stagedPatch": "", "modelSelection": sel(instance, models[0], Value::Null)}}),
        json!({"op": "generateBranchName", "fake": answer(json!({"title": "not a branch"})), "input": {
            "message": "x", "modelSelection": sel(instance, models[0], Value::Null)}}),
        json!({"op": "generateBranchName", "fake": {"exit": 2}, "input": {
            "message": "x", "modelSelection": sel(instance, models[0], Value::Null)}}),
    ]
}

#[tokio::test]
async fn claude_cli_calls_match_the_typescript_server() {
    if let Err(why) = oracle_available() {
        eprintln!("skipping the TS golden comparison: {why}");
        return;
    }
    let mut claude_calls = calls(
        "claudeAgent",
        [CAPABLE, THINKING, COLLIDING_ALIAS],
        [
            json!([{"id": "effort", "value": "max"}, {"id": "fastMode", "value": true}]),
            json!([{"id": "thinking", "value": false}, {"id": "effort", "value": "high"}]),
            json!([{"id": "effort", "value": "ultrathink"}, {"id": "contextWindow", "value": "standard"}]),
        ],
        |fields| json!({"stdout": json!({"structured_output": fields}).to_string()}),
    );
    claude_calls.push(json!({"op": "generateThreadTitle", "fake": {"stdout": "not json"}, "input": {
        "message": "x", "modelSelection": sel("claudeAgent", STANDARD, Value::Null)}}));
    claude_calls.push(json!({"op": "generateThreadTitle", "fake": {"stdout": json!([{"type": "system"}, {"type": "result", "structured_output": {"title": "Verbose made-up"}}]).to_string()}, "input": {
        "message": "x", "modelSelection": sel("claudeAgent", STANDARD, Value::Null)}}));
    compare(Run {
        provider: "claude",
        config: json!({"homePath": "claude-home", "customModels": [COLLIDING_ALIAS]}),
        extra_env: Vec::new(),
        models: Vec::new(),
        calls: claude_calls,
    })
    .await;
}

#[tokio::test]
async fn codex_cli_calls_match_the_typescript_server() {
    if let Err(why) = oracle_available() {
        eprintln!("skipping the TS golden comparison: {why}");
        return;
    }
    let answer = |fields: Value| json!({"output": fields.to_string()});
    let codex_calls = calls(
        "codex",
        ["gpt-5.6-luna", "gpt-5.4", "made-up-model"],
        [
            json!([{"id": "reasoningEffort", "value": "xhigh"}, {"id": "serviceTier", "value": "priority"}]),
            json!([{"id": "fastMode", "value": true}]),
            Value::Null,
        ],
        answer,
    );
    compare(Run {
        provider: "codex",
        config: json!({"homePath": "codex-home", "launchArgs": "--strict-config --enable made-up-feature --listen off -c model=\"x\""}),
        extra_env: Vec::new(),
        models: vec!["openai.gpt-5.6-luna", "gpt-5.4"],
        calls: codex_calls.clone(),
    })
    .await;
    compare(Run {
        provider: "codex",
        config: json!({"launchArgs": "--enable settings-feature"}),
        extra_env: vec![("T3CODE_CODEX_LAUNCH_ARGS", " --strict-config --disable made-up-off ")],
        models: Vec::new(),
        calls: codex_calls,
    })
    .await;
}
