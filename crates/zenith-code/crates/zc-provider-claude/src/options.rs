//! What the Agent SDK does with its `Options` before the CLI starts, without the SDK
//! (`@anthropic-ai/claude-agent-sdk` 0.3.276, `sdk.mjs`: `k0`, `ProcessTransport.initialize`,
//! `Query.initialize`):
//!
//! - [`ClaudeQueryOptions`]: the subset of `Options` zenith code sets (adapter sessions and the
//!   status probe), with callbacks reduced to whether they are present;
//! - [`build_spawn_spec`]: the exact command, argv and environment the SDK spawns;
//! - [`initialize_request`]: the `initialize` control request it sends first.
//!
//! The argv is checked against the SDK's own builder (`tests/sdk_argv.rs`, gate 3).

use serde_json::{Map, Value};

use crate::home::Env;
use crate::usage_limits::js_number;

/// The SDK version zenith code pins (`CLAUDE_AGENT_SDK_VERSION`).
pub const CLAUDE_AGENT_SDK_VERSION: &str = "0.3.276";

/// `Options.systemPrompt`.
#[derive(Debug, Clone, PartialEq)]
pub enum SystemPrompt {
    /// A replacement prompt.
    Custom(String),
    /// `{type: "preset", preset: "claude_code", append?}`.
    Preset { append: Option<String> },
}

/// `Options.thinking`.
#[derive(Debug, Clone, PartialEq)]
pub enum ThinkingConfig {
    Adaptive { display: Option<String> },
    Enabled { budget_tokens: Option<u64>, display: Option<String> },
    Disabled,
}

/// The SDK `Options` zenith code uses. `env` is the complete child environment the caller wants
/// (the SDK copies `process.env` only when none is given; zenith always gives one).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaudeQueryOptions {
    pub cwd: Option<String>,
    pub model: Option<String>,
    /// `pathToClaudeCodeExecutable`: the instance's `binaryPath`, resolved.
    pub path_to_claude_code_executable: String,
    /// The JS runtime for a `.js` entry point (`node` by default).
    pub executable: Option<String>,
    pub executable_args: Vec<String>,
    pub system_prompt: Option<SystemPrompt>,
    pub setting_sources: Option<Vec<String>>,
    pub effort: Option<String>,
    pub thinking: Option<ThinkingConfig>,
    pub max_turns: Option<u32>,
    /// `permissionMode`; the SDK sends `default` when unset.
    pub permission_mode: Option<String>,
    pub allow_dangerously_skip_permissions: bool,
    /// `settings` as an object (serialized with `JSON.stringify` key order).
    pub settings: Option<Map<String, Value>>,
    pub resume: Option<String>,
    pub resume_session_at: Option<String>,
    pub fork_session: bool,
    pub session_id: Option<String>,
    pub persist_session: Option<bool>,
    pub include_partial_messages: bool,
    /// Whether a `canUseTool` callback is installed (`--permission-prompt-tool stdio`).
    pub can_use_tool: bool,
    /// Whether an `onUserDialog` callback is installed.
    pub on_user_dialog: bool,
    pub supported_dialog_kinds: Option<Vec<String>>,
    pub env: Env,
    pub additional_directories: Vec<String>,
    /// `extraArgs`: flag → value, `Value::Null` for a bare flag, in insertion order.
    pub extra_args: Map<String, Value>,
    pub mcp_servers: Option<Map<String, Value>>,
    pub strict_mcp_config: bool,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
}

/// What to spawn.
#[derive(Debug, Clone, PartialEq)]
pub struct SpawnSpec {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: Env,
}

/// `CR(argv, key, value)`: `--key=value` when the value looks like a flag, else two items.
fn push_flag_value(argv: &mut Vec<String>, key: &str, value: &str) {
    if value.chars().count() > 1 && value.starts_with('-') {
        argv.push(format!("--{key}={value}"));
    } else {
        argv.push(format!("--{key}"));
        argv.push(value.to_string());
    }
}

/// JS `String(value)` for an `extraArgs` value.
fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => js_number(n.as_f64().unwrap_or(0.0)).to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Array(items) => items
            .iter()
            .map(|item| if item.is_null() { String::new() } else { js_string(item) })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
        Value::Null => "null".into(),
    }
}

/// `hze(path)`: a native binary unless it is a JS/TS entry point.
pub fn is_native_executable(path: &str) -> bool {
    ![".js", ".mjs", ".tsx", ".ts", ".jsx"].iter().any(|ext| path.ends_with(ext))
}

/// JS truthiness of an env var for `Pe(...)`.
fn env_truthy(value: Option<&String>) -> bool {
    value.is_some_and(|v| matches!(v.to_lowercase().trim(), "1" | "true" | "yes" | "on"))
}

/// The CLI arguments (`ProcessTransport.initialize`'s `q`), without the executable.
pub fn build_cli_args(options: &ClaudeQueryOptions) -> Vec<String> {
    let mut argv: Vec<String> = ["--output-format", "stream-json", "--verbose", "--input-format", "stream-json"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if let Some(thinking) = &options.thinking {
        let display = match thinking {
            ThinkingConfig::Enabled { budget_tokens: None, display } => {
                argv.extend(["--thinking".into(), "adaptive".into()]);
                display
            }
            ThinkingConfig::Enabled {
                budget_tokens: Some(tokens),
                display,
            } => {
                argv.extend(["--max-thinking-tokens".into(), tokens.to_string()]);
                display
            }
            ThinkingConfig::Disabled => {
                argv.extend(["--thinking".into(), "disabled".into()]);
                &None
            }
            ThinkingConfig::Adaptive { display } => {
                argv.extend(["--thinking".into(), "adaptive".into()]);
                display
            }
        };
        if let Some(display) = display.as_deref().filter(|d| !d.is_empty()) {
            argv.extend(["--thinking-display".into(), display.to_string()]);
        }
    }
    if let Some(effort) = options.effort.as_deref().filter(|e| !e.is_empty()) {
        argv.extend(["--effort".into(), effort.to_string()]);
    }
    if let Some(max_turns) = options.max_turns.filter(|n| *n > 0) {
        argv.extend(["--max-turns".into(), max_turns.to_string()]);
    }
    if let Some(model) = options.model.as_deref().filter(|m| !m.is_empty()) {
        argv.extend(["--model".into(), model.to_string()]);
    }
    if options.can_use_tool {
        argv.extend(["--permission-prompt-tool".into(), "stdio".into()]);
    }
    if let Some(resume) = options.resume.as_deref().filter(|r| !r.is_empty()) {
        argv.push(format!("--resume={resume}"));
    }
    if !options.allowed_tools.is_empty() {
        argv.extend(["--allowedTools".into(), options.allowed_tools.join(",")]);
    }
    if !options.disallowed_tools.is_empty() {
        argv.extend(["--disallowedTools".into(), options.disallowed_tools.join(",")]);
    }
    if let Some(servers) = options.mcp_servers.as_ref().filter(|servers| !servers.is_empty()) {
        let mut wrapper = Map::new();
        wrapper.insert("mcpServers".into(), Value::Object(servers.clone()));
        argv.extend(["--mcp-config".into(), Value::Object(wrapper).to_string()]);
    }
    if let Some(sources) = &options.setting_sources {
        argv.push(format!("--setting-sources={}", sources.join(",")));
    }
    if options.strict_mcp_config {
        argv.push("--strict-mcp-config".into());
    }
    let permission_mode = options.permission_mode.as_deref().filter(|m| !m.is_empty()).unwrap_or("default");
    argv.extend(["--permission-mode".into(), permission_mode.to_string()]);
    if options.allow_dangerously_skip_permissions {
        argv.push("--allow-dangerously-skip-permissions".into());
    }
    if options.include_partial_messages {
        argv.push("--include-partial-messages".into());
    }
    for directory in &options.additional_directories {
        argv.extend(["--add-dir".into(), directory.clone()]);
    }
    if options.fork_session {
        argv.push("--fork-session".into());
    }
    if let Some(at) = options.resume_session_at.as_deref().filter(|r| !r.is_empty()) {
        argv.push(format!("--resume-session-at={at}"));
    }
    if let Some(session_id) = options.session_id.as_deref().filter(|s| !s.is_empty()) {
        argv.push(format!("--session-id={session_id}"));
    }
    if options.persist_session == Some(false) {
        argv.push("--no-session-persistence".into());
    }
    let mut extra = options.extra_args.clone();
    if let Some(settings) = &options.settings {
        extra.insert("settings".into(), Value::String(Value::Object(settings.clone()).to_string()));
    }
    for (key, value) in &extra {
        if value.is_null() {
            argv.push(format!("--{key}"));
        } else {
            push_flag_value(&mut argv, key, &js_string(value));
        }
    }
    argv
}

/// `k0` + `ProcessTransport.initialize`: command, argv, cwd and environment.
pub fn build_spawn_spec(options: &ClaudeQueryOptions) -> SpawnSpec {
    let mut env = options.env.clone();
    if env.get("CLAUDE_CODE_ENTRYPOINT").is_none_or(|v| v.is_empty()) {
        env.insert("CLAUDE_CODE_ENTRYPOINT".into(), "sdk-ts".into());
    }
    if env.get("CLAUDE_AGENT_SDK_VERSION").is_none_or(|v| v.is_empty()) {
        env.insert("CLAUDE_AGENT_SDK_VERSION".into(), CLAUDE_AGENT_SDK_VERSION.into());
    }
    env.remove("NODE_OPTIONS");
    if env_truthy(env.get("DEBUG_CLAUDE_AGENT_SDK")) {
        env.insert("DEBUG".into(), "1".into());
    } else {
        env.remove("DEBUG");
    }
    let cli_args = build_cli_args(options);
    let path = options.path_to_claude_code_executable.clone();
    let (command, args) = if is_native_executable(&path) {
        (path, options.executable_args.iter().cloned().chain(cli_args).collect())
    } else {
        let executable = options.executable.clone().unwrap_or_else(|| "node".into());
        (
            executable,
            options.executable_args.iter().cloned().chain(std::iter::once(path)).chain(cli_args).collect(),
        )
    };
    SpawnSpec {
        command,
        args,
        cwd: options.cwd.clone(),
        env,
    }
}

/// The `initialize` control request (`Query.initialize`), fields the SDK would leave
/// `undefined` omitted.
pub fn initialize_request(options: &ClaudeQueryOptions) -> Value {
    let mut request = Map::new();
    request.insert("subtype".into(), Value::String("initialize".into()));
    match &options.system_prompt {
        None => {
            request.insert("systemPrompt".into(), Value::Array(vec![Value::String(String::new())]));
        }
        Some(SystemPrompt::Custom(prompt)) => {
            request.insert("systemPrompt".into(), Value::Array(vec![Value::String(prompt.clone())]));
        }
        Some(SystemPrompt::Preset { append }) => {
            if let Some(append) = append {
                request.insert("appendSystemPrompt".into(), Value::String(append.clone()));
            }
        }
    }
    if let Some(kinds) = &options.supported_dialog_kinds {
        request.insert("supportedDialogKinds".into(), Value::Array(kinds.iter().cloned().map(Value::String).collect()));
    }
    Value::Object(request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builds_the_sdk_argv_for_an_adapter_session() {
        let mut extra = Map::new();
        extra.insert("verbose".into(), Value::Null);
        extra.insert("thinking-display".into(), json!("summarized"));
        let options = ClaudeQueryOptions {
            cwd: Some("/work".into()),
            model: Some("claude-x[1m]".into()),
            path_to_claude_code_executable: "claude".into(),
            system_prompt: Some(SystemPrompt::Preset { append: Some("hi".into()) }),
            setting_sources: Some(vec!["user".into(), "project".into(), "local".into()]),
            effort: Some("high".into()),
            thinking: Some(ThinkingConfig::Adaptive {
                display: Some("summarized".into()),
            }),
            permission_mode: Some("bypassPermissions".into()),
            allow_dangerously_skip_permissions: true,
            settings: Some(serde_json::from_value(json!({"showThinkingSummaries": true, "autoCompactWindow": 300000})).unwrap()),
            session_id: Some("00000000-0000-4000-8000-000000000001".into()),
            include_partial_messages: true,
            can_use_tool: true,
            on_user_dialog: true,
            supported_dialog_kinds: Some(vec!["resume_return".into()]),
            env: Env::from([("NODE_OPTIONS".to_string(), "--x".to_string()), ("DEBUG".to_string(), "1".to_string())]),
            additional_directories: vec!["/work".into(), "/att".into()],
            extra_args: extra,
            ..ClaudeQueryOptions::default()
        };
        let spec = build_spawn_spec(&options);
        assert_eq!(spec.command, "claude");
        assert_eq!(
            spec.args,
            vec![
                "--output-format",
                "stream-json",
                "--verbose",
                "--input-format",
                "stream-json",
                "--thinking",
                "adaptive",
                "--thinking-display",
                "summarized",
                "--effort",
                "high",
                "--model",
                "claude-x[1m]",
                "--permission-prompt-tool",
                "stdio",
                "--setting-sources=user,project,local",
                "--permission-mode",
                "bypassPermissions",
                "--allow-dangerously-skip-permissions",
                "--include-partial-messages",
                "--add-dir",
                "/work",
                "--add-dir",
                "/att",
                "--session-id=00000000-0000-4000-8000-000000000001",
                "--verbose",
                "--thinking-display",
                "summarized",
                "--settings",
                r#"{"showThinkingSummaries":true,"autoCompactWindow":300000}"#
            ]
        );
        assert_eq!(spec.env.get("CLAUDE_CODE_ENTRYPOINT").map(String::as_str), Some("sdk-ts"));
        assert_eq!(spec.env.get("CLAUDE_AGENT_SDK_VERSION").map(String::as_str), Some(CLAUDE_AGENT_SDK_VERSION));
        assert!(!spec.env.contains_key("NODE_OPTIONS") && !spec.env.contains_key("DEBUG"));
        assert_eq!(
            initialize_request(&options),
            json!({"subtype": "initialize", "appendSystemPrompt": "hi", "supportedDialogKinds": ["resume_return"]})
        );
    }

    #[test]
    fn runs_js_entry_points_through_node_and_passes_dash_values_inline() {
        let mut extra = Map::new();
        extra.insert("append".into(), json!("-x"));
        let options = ClaudeQueryOptions {
            path_to_claude_code_executable: "/pkg/cli.js".into(),
            extra_args: extra,
            ..ClaudeQueryOptions::default()
        };
        let spec = build_spawn_spec(&options);
        assert_eq!(spec.command, "node");
        assert_eq!(spec.args[0], "/pkg/cli.js");
        assert_eq!(spec.args.last().unwrap(), "--append=-x");
        assert!(spec.args.windows(2).any(|w| w == ["--permission-mode", "default"]));
        assert_eq!(initialize_request(&options), json!({"subtype": "initialize", "systemPrompt": [""]}));
    }
}
