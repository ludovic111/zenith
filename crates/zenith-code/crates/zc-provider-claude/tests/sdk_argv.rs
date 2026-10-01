//! Gate 3: the command, argv, environment and `initialize` request match what the Agent SDK
//! (0.3.276) itself builds, for hand-picked option combinations and for the options the adapter
//! and the status probe really produce.
//!
//! `tests/fixtures/sdk-argv.json` is generated from the SDK:
//!   ZC_SDK_ARGV_CASES=/tmp/cases.json cargo test -p zc-provider-claude --test sdk_argv
//!   node tests/fixtures/sdk-argv.mjs <path to @anthropic-ai/claude-agent-sdk> /tmp/cases.json tests/fixtures/sdk-argv.json
//! The test fails when a case's SDK options drift from the fixture, so the fixture stays honest.

mod support;

use serde_json::{json, Map, Value};
use support::*;
use zc_provider_claude::home::Env;
use zc_provider_claude::options::{build_spawn_spec, initialize_request, ClaudeQueryOptions, SystemPrompt, ThinkingConfig};

/// The SDK `Options` object for `options` (callbacks as `true`, replaced by functions in node).
fn to_sdk_json(options: &ClaudeQueryOptions) -> Value {
    let mut out = Map::new();
    let mut put = |key: &str, value: Value| {
        out.insert(key.to_string(), value);
    };
    if let Some(cwd) = &options.cwd {
        put("cwd", json!(cwd));
    }
    if let Some(model) = &options.model {
        put("model", json!(model));
    }
    put("pathToClaudeCodeExecutable", json!(options.path_to_claude_code_executable));
    if let Some(executable) = &options.executable {
        put("executable", json!(executable));
    }
    if !options.executable_args.is_empty() {
        put("executableArgs", json!(options.executable_args));
    }
    match &options.system_prompt {
        Some(SystemPrompt::Custom(prompt)) => put("systemPrompt", json!(prompt)),
        Some(SystemPrompt::Preset { append }) => {
            let mut preset = json!({"type": "preset", "preset": "claude_code"});
            if let Some(append) = append {
                preset["append"] = json!(append);
            }
            put("systemPrompt", preset);
        }
        None => {}
    }
    if let Some(sources) = &options.setting_sources {
        put("settingSources", json!(sources));
    }
    if let Some(effort) = &options.effort {
        put("effort", json!(effort));
    }
    if let Some(thinking) = &options.thinking {
        let value = match thinking {
            ThinkingConfig::Adaptive { display } => json!({"type": "adaptive", "display": display}),
            ThinkingConfig::Enabled { budget_tokens, display } => json!({"type": "enabled", "budgetTokens": budget_tokens, "display": display}),
            ThinkingConfig::Disabled => json!({"type": "disabled"}),
        };
        let cleaned: Map<String, Value> = value
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, v)| !v.is_null())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        put("thinking", Value::Object(cleaned));
    }
    if let Some(max_turns) = options.max_turns {
        put("maxTurns", json!(max_turns));
    }
    if let Some(mode) = &options.permission_mode {
        put("permissionMode", json!(mode));
    }
    if options.allow_dangerously_skip_permissions {
        put("allowDangerouslySkipPermissions", json!(true));
    }
    if let Some(settings) = &options.settings {
        put("settings", Value::Object(settings.clone()));
    }
    if let Some(resume) = &options.resume {
        put("resume", json!(resume));
    }
    if let Some(at) = &options.resume_session_at {
        put("resumeSessionAt", json!(at));
    }
    if options.fork_session {
        put("forkSession", json!(true));
    }
    if let Some(id) = &options.session_id {
        put("sessionId", json!(id));
    }
    if let Some(persist) = options.persist_session {
        put("persistSession", json!(persist));
    }
    if options.include_partial_messages {
        put("includePartialMessages", json!(true));
    }
    if options.can_use_tool {
        put("canUseTool", json!(true));
    }
    if options.on_user_dialog {
        put("onUserDialog", json!(true));
    }
    if let Some(kinds) = &options.supported_dialog_kinds {
        put("supportedDialogKinds", json!(kinds));
    }
    put("env", json!(options.env));
    if !options.additional_directories.is_empty() {
        put("additionalDirectories", json!(options.additional_directories));
    }
    if !options.extra_args.is_empty() {
        put("extraArgs", Value::Object(options.extra_args.clone()));
    }
    if let Some(servers) = &options.mcp_servers {
        put("mcpServers", Value::Object(servers.clone()));
    }
    if options.strict_mcp_config {
        put("strictMcpConfig", json!(true));
    }
    if !options.allowed_tools.is_empty() {
        put("allowedTools", json!(options.allowed_tools));
    }
    if !options.disallowed_tools.is_empty() {
        put("disallowedTools", json!(options.disallowed_tools));
    }
    Value::Object(out)
}

fn map(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

fn base_env() -> Env {
    Env::from([
        ("PATH".to_string(), "/usr/bin:/bin".to_string()),
        ("HOME".to_string(), "/home/test-user".to_string()),
    ])
}

fn manual_cases() -> Vec<(&'static str, ClaudeQueryOptions)> {
    let base = ClaudeQueryOptions {
        path_to_claude_code_executable: "/opt/test/claude".into(),
        env: base_env(),
        ..ClaudeQueryOptions::default()
    };
    vec![
        ("minimal", base.clone()),
        (
            "everything",
            ClaudeQueryOptions {
                cwd: Some("/work/project".into()),
                model: Some("claude-test-1[1m]".into()),
                system_prompt: Some(SystemPrompt::Preset {
                    append: Some("Be brief.\nAlways.".into()),
                }),
                setting_sources: Some(vec!["user".into(), "project".into(), "local".into()]),
                effort: Some("xhigh".into()),
                thinking: Some(ThinkingConfig::Adaptive {
                    display: Some("summarized".into()),
                }),
                max_turns: Some(1),
                permission_mode: Some("bypassPermissions".into()),
                allow_dangerously_skip_permissions: true,
                settings: Some(map(
                    json!({"ultracode": true, "autoCompactWindow": 300000, "nested": {"b": 1, "a": [1, "two"]}}),
                )),
                session_id: Some("00000000-0000-4000-8000-000000000001".into()),
                include_partial_messages: true,
                can_use_tool: true,
                on_user_dialog: true,
                supported_dialog_kinds: Some(vec!["resume_return".into()]),
                additional_directories: vec!["/work/project".into(), "/data/attachments".into()],
                extra_args: map(json!({"thinking-display": "summarized", "debug-to-stderr": null, "append-flag": "-x", "count": 3, "dash": "-"})),
                mcp_servers: Some(map(
                    json!({"zenith-code": {"type": "http", "url": "http://127.0.0.1:4000/mcp", "headers": {"Authorization": "Bearer test-token"}}}),
                )),
                allowed_tools: vec!["Read".into(), "Grep".into()],
                disallowed_tools: vec!["Bash(rm:*)".into()],
                env: Env::from([
                    ("PATH".to_string(), "/usr/bin".to_string()),
                    ("NODE_OPTIONS".to_string(), "--inspect".to_string()),
                    ("DEBUG".to_string(), "1".to_string()),
                    ("CLAUDE_CODE_ENTRYPOINT".to_string(), "".to_string()),
                ]),
                ..base.clone()
            },
        ),
        (
            "resume-fork",
            ClaudeQueryOptions {
                resume: Some("550e8400-e29b-41d4-a716-446655440000".into()),
                resume_session_at: Some("assistant-uuid-9".into()),
                fork_session: true,
                persist_session: Some(false),
                permission_mode: Some("plan".into()),
                system_prompt: Some(SystemPrompt::Custom("You are a test.".into())),
                thinking: Some(ThinkingConfig::Enabled {
                    budget_tokens: Some(4096),
                    display: Some("omitted".into()),
                }),
                env: Env::from([
                    ("CLAUDE_CODE_ENTRYPOINT".to_string(), "custom-entry".to_string()),
                    ("DEBUG_CLAUDE_AGENT_SDK".to_string(), "true".to_string()),
                    ("DEBUG".to_string(), "x".to_string()),
                ]),
                ..base.clone()
            },
        ),
        (
            "thinking-disabled",
            ClaudeQueryOptions {
                thinking: Some(ThinkingConfig::Disabled),
                system_prompt: Some(SystemPrompt::Preset { append: None }),
                ..base.clone()
            },
        ),
        (
            "thinking-enabled-no-budget",
            ClaudeQueryOptions {
                thinking: Some(ThinkingConfig::Enabled {
                    budget_tokens: None,
                    display: None,
                }),
                ..base.clone()
            },
        ),
        (
            "js-entry-point",
            ClaudeQueryOptions {
                path_to_claude_code_executable: "/opt/test/node_modules/claude/cli.js".into(),
                executable: Some("bun".into()),
                executable_args: vec!["--smol".into()],
                strict_mcp_config: true,
                mcp_servers: Some(Map::new()),
                ..base.clone()
            },
        ),
        (
            "mjs-default-runtime",
            ClaudeQueryOptions {
                path_to_claude_code_executable: "/opt/test/cli.mjs".into(),
                ..base.clone()
            },
        ),
    ]
}

async fn adapter_cases() -> Vec<(&'static str, ClaudeQueryOptions)> {
    let mut cases = Vec::new();
    let env = || Some(base_env());
    let harness = Harness::new(HarnessConfig {
        attachments_dir: Some("/data/attachments".into()),
        environment: env(),
        claude_config: Some(json!({"binaryPath": "/opt/test/claude"})),
        ..HarnessConfig::default()
    });
    harness.start(json!({"cwd": "/work/project"})).await;
    cases.push(("adapter-full-access", harness.factory.last().options.clone()));

    let harness = Harness::new(HarnessConfig {
        attachments_dir: Some("/data/attachments".into()),
        environment: env(),
        claude_config: Some(json!({"binaryPath": "/opt/test/claude", "homePath": "/home/test-user/.claude-alt"})),
        ..HarnessConfig::default()
    });
    harness
        .start(json!({"runtimeMode": "approval-required", "cwd": "/work/project", "resumeCursor": {"resume": "550e8400-e29b-41d4-a716-446655440000"},
            "modelSelection": {"instanceId": "claudeAgent", "model": CAPABLE, "options": [{"id": "effort", "value": "max"}, {"id": "thinking", "value": false}, {"id": "contextWindow", "value": "expanded"}]}}))
        .await;
    cases.push(("adapter-approval-resume-model", harness.factory.last().options.clone()));

    let harness = Harness::new(HarnessConfig {
        attachments_dir: Some("/data/attachments".into()),
        environment: env(),
        claude_config: Some(json!({"binaryPath": "/opt/test/claude", "launchArgs": "--chrome --debug-file /tmp/x"})),
        ..HarnessConfig::default()
    });
    harness.start(json!({"runtimeMode": "auto-accept-edits", "modelSelection": {"instanceId": "claudeAgent", "model": THINKING, "options": [{"id": "effort", "value": "ultracode"}]}})).await;
    cases.push(("adapter-auto-accept-launch-args", harness.factory.last().options.clone()));

    cases.push((
        "status-probe",
        zc_provider_claude::provider::capabilities_probe_options("/opt/test/claude", &base_env(), Some("/work/project")),
    ));
    cases
}

#[tokio::test]
async fn matches_the_agent_sdk_builder() {
    let mut cases = manual_cases();
    cases.extend(adapter_cases().await);
    let rendered: Vec<Value> = cases
        .iter()
        .map(|(name, options)| json!({"name": name, "sdkOptions": to_sdk_json(options)}))
        .collect();
    if let Ok(path) = std::env::var("ZC_SDK_ARGV_CASES") {
        std::fs::write(path, serde_json::to_string_pretty(&rendered).unwrap()).unwrap();
        return;
    }
    let fixture: Vec<Value> = serde_json::from_str(include_str!("fixtures/sdk-argv.json")).unwrap();
    assert_eq!(fixture.len(), cases.len(), "regenerate tests/fixtures/sdk-argv.json");
    for ((name, options), (expected, current)) in cases.iter().zip(fixture.iter().zip(rendered.iter())) {
        assert_eq!(expected["name"], json!(name));
        assert_eq!(
            expected["sdkOptions"], current["sdkOptions"],
            "{name}: the SDK options drifted from the fixture; regenerate it"
        );
        let spec = build_spawn_spec(options);
        assert_eq!(json!(spec.command), expected["command"], "{name}: command");
        assert_eq!(json!(spec.args), expected["args"], "{name}: argv");
        assert_eq!(json!(spec.env), expected["env"], "{name}: env");
        assert_eq!(spec.cwd.map(Value::String).unwrap_or(Value::Null), expected["cwd"], "{name}: cwd");
        assert_eq!(initialize_request(options), expected["initialize"], "{name}: initialize");
    }
}
