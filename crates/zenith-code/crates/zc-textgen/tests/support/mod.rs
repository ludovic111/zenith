//! Shared test support: a recording fake CLI, the synthetic Claude catalog, port inputs.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use zc_ports::contracts::{ChatAttachment, ModelSelection};
use zc_ports::text_generation::{BranchNameGenerationInput, CommitMessageGenerationInput, PrContentGenerationInput, ThreadTitleGenerationInput};
use zc_provider_claude::catalog::{ClaudeCatalogModel, ClaudeCodeCompatibility, ClaudeCodeProfile};
use zc_provider_claude::ClaudeModelCatalog;

pub const CAPABLE: &str = "claude-synthetic-capable";
pub const COLLIDING_ALIAS: &str = "synthetic-collision";
pub const STANDARD: &str = "claude-synthetic-standard";
pub const THINKING: &str = "claude-synthetic-thinking";

pub const FAKE_CLI_SOURCE: &str = include_str!("fake_cli.sh");

/// A fake `claude` / `codex` that records each call (see `fake_cli.sh`).
pub struct FakeCli {
    pub root: tempfile::TempDir,
    pub binary: PathBuf,
    pub data: PathBuf,
}

/// One recorded call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: String,
    pub stdin: String,
    pub schema: Option<String>,
}

impl Record {
    pub fn args(&self) -> String {
        self.argv.join(" ")
    }

    pub fn value_after(&self, flag: &str) -> Option<&str> {
        let index = self.argv.iter().position(|arg| arg == flag)?;
        self.argv.get(index + 1).map(String::as_str)
    }

    pub fn has(&self, arg: &str) -> bool {
        self.argv.iter().any(|candidate| candidate == arg)
    }
}

impl FakeCli {
    pub fn new(name: &str) -> Self {
        let root = tempfile::Builder::new().prefix("zc-textgen-fake-").tempdir().unwrap();
        let bin = root.path().join("bin");
        let data = root.path().join("data");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let binary = bin.join(name);
        std::fs::write(&binary, FAKE_CLI_SOURCE).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { root, binary, data }
    }

    pub fn stdout(&self, text: &str) -> &Self {
        std::fs::write(self.data.join("stdout"), text).unwrap();
        self
    }

    pub fn stderr(&self, text: &str) -> &Self {
        std::fs::write(self.data.join("stderr"), text).unwrap();
        self
    }

    /// What Codex writes to `--output-last-message` (with the trailing newline the TS fake adds).
    pub fn output(&self, text: &str) -> &Self {
        std::fs::write(self.data.join("output"), format!("{text}\n")).unwrap();
        self
    }

    pub fn exit(&self, code: i32) -> &Self {
        std::fs::write(self.data.join("exit"), code.to_string()).unwrap();
        self
    }

    /// The environment the instance runs the CLI with.
    pub fn environment(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ("FAKE_CLI_DIR".to_owned(), self.data.to_string_lossy().into_owned()),
            ("ZC_TEXTGEN_MARKER".to_owned(), "made-up-value".to_owned()),
        ])
    }

    pub fn records(&self) -> Vec<Record> {
        let mut records = Vec::new();
        for n in 1.. {
            let dir = self.data.join(format!("record-{n}"));
            if !dir.exists() {
                break;
            }
            records.push(read_record(&dir));
        }
        records
    }

    pub fn only_record(&self) -> Record {
        let records = self.records();
        assert_eq!(records.len(), 1, "expected one call, got {records:?}");
        records.into_iter().next().unwrap()
    }
}

fn read_record(dir: &Path) -> Record {
    let read = |name: &str| std::fs::read(dir.join(name)).unwrap_or_default();
    let split = |bytes: Vec<u8>| -> Vec<String> {
        let mut parts: Vec<String> = bytes.split(|byte| *byte == 0).map(|part| String::from_utf8_lossy(part).into_owned()).collect();
        if parts.last().is_some_and(String::is_empty) {
            parts.pop();
        }
        parts
    };
    let env = split(read("env"))
        .into_iter()
        .filter_map(|entry| entry.split_once('=').map(|(key, value)| (key.to_owned(), value.to_owned())))
        .collect();
    Record {
        argv: split(read("argv")),
        env,
        cwd: String::from_utf8_lossy(&read("cwd")).trim_end().to_owned(),
        stdin: String::from_utf8_lossy(&read("stdin")).into_owned(),
        schema: std::fs::read_to_string(dir.join("schema")).ok(),
    }
}

/// `SYNTHETIC_CLAUDE_MODEL_CATALOG` (`provider/ClaudeModelCatalog.testFixtures.ts`).
pub fn synthetic_catalog() -> ClaudeModelCatalog {
    let effort = json!({
        "id": "effort", "label": "Reasoning", "type": "select",
        "options": [{"id": "low", "label": "Low"}, {"id": "high", "label": "High", "isDefault": true}, {"id": "max", "label": "Max"}, {"id": "ultrathink", "label": "Ultrathink"}],
        "promptInjectedValues": ["ultrathink"]
    });
    let context_window = json!({
        "id": "contextWindow", "label": "Context Window", "type": "select",
        "options": [{"id": "standard", "label": "Standard"}, {"id": "expanded", "label": "Expanded", "isDefault": true}]
    });
    let runtime = ClaudeCodeProfile {
        effort_map: Some(BTreeMap::from([("ultrathink".to_string(), None)])),
        model_suffixes: Some(vec![(
            "contextWindow".into(),
            BTreeMap::from([("expanded".to_string(), "[expanded]".to_string())]),
        )]),
        context_window_tokens: Some(BTreeMap::from([("standard".to_string(), 200_000.0), ("expanded".to_string(), 1_000_000.0)])),
        fixed_context_window_tokens: None,
    };
    ClaudeModelCatalog {
        models: vec![
            ClaudeCatalogModel {
                model: json!({"slug": CAPABLE, "name": "Claude Synthetic Capable", "aliases": [COLLIDING_ALIAS], "isCustom": false,
                    "capabilities": {"optionDescriptors": [effort.clone(), {"id": "fastMode", "label": "Fast Mode", "type": "boolean"}, context_window.clone()]}}),
                runtime: runtime.clone(),
                compatibility: ClaudeCodeCompatibility::default(),
            },
            ClaudeCatalogModel {
                model: json!({"slug": STANDARD, "name": "Claude Synthetic Standard", "isCustom": false, "capabilities": {"optionDescriptors": [effort, context_window]}}),
                runtime,
                compatibility: ClaudeCodeCompatibility::default(),
            },
            ClaudeCatalogModel {
                model: json!({"slug": THINKING, "name": "Claude Synthetic Thinking", "isCustom": false,
                    "capabilities": {"optionDescriptors": [{"id": "thinking", "label": "Thinking", "type": "boolean"}]}}),
                runtime: ClaudeCodeProfile::default(),
                compatibility: ClaudeCodeCompatibility::default(),
            },
        ],
    }
}

/// `createModelSelection(instanceId, model, options?)`.
pub fn selection(instance_id: &str, model: &str, options: Option<Value>) -> ModelSelection {
    let mut value = json!({"instanceId": instance_id, "model": model});
    if let Some(options) = options {
        value["options"] = options;
    }
    ModelSelection(value)
}

pub fn project_cwd() -> String {
    std::env::current_dir().unwrap().to_string_lossy().into_owned()
}

pub fn commit_input(model_selection: ModelSelection) -> CommitMessageGenerationInput {
    CommitMessageGenerationInput {
        cwd: project_cwd(),
        branch: Some("feature/made-up-change".into()),
        staged_summary: "M README.md".into(),
        staged_patch: "diff --git a/README.md b/README.md".into(),
        include_branch: false,
        policy: None,
        model_selection,
    }
}

pub fn pr_input(model_selection: ModelSelection) -> PrContentGenerationInput {
    PrContentGenerationInput {
        cwd: project_cwd(),
        base_branch: "main".into(),
        head_branch: "feature/made-up-change".into(),
        commit_summary: "Improve orchestration".into(),
        diff_summary: "1 file changed".into(),
        diff_patch: "diff --git a/README.md b/README.md".into(),
        change_request_template: None,
        policy: None,
        model_selection,
    }
}

pub fn branch_input(message: &str, model_selection: ModelSelection) -> BranchNameGenerationInput {
    BranchNameGenerationInput {
        cwd: project_cwd(),
        message: message.into(),
        attachments: Vec::new(),
        model_selection,
    }
}

pub fn title_input(message: &str, model_selection: ModelSelection) -> ThreadTitleGenerationInput {
    ThreadTitleGenerationInput {
        linked_context: None,
        cwd: project_cwd(),
        message: message.into(),
        previous_title: None,
        attachments: Vec::new(),
        model_selection,
    }
}

pub fn image_attachment(id: &str, name: &str) -> ChatAttachment {
    ChatAttachment(json!({"type": "image", "id": id, "name": name, "mimeType": "image/png", "sizeBytes": 5}))
}

pub fn detail(error: &zc_ports::TaggedError) -> String {
    error.fields.get("detail").and_then(Value::as_str).unwrap_or_default().to_owned()
}

pub fn env_map(map: &BTreeMap<String, String>) -> HashMap<String, String> {
    map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}
