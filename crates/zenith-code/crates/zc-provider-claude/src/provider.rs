//! `provider/Layers/ClaudeProvider.ts`: the Claude status probe behind the provider snapshot.
//! `claude --version`, then a capabilities probe (a CLI session whose prompt never yields, so
//! nothing reaches the API: its `initialize` response carries the account and the slash
//! commands, and a `get_usage` control request the subscription windows), the skills on disk,
//! and the banked resets.
//!
//! The result is a `ServerProviderDraft` (`ServerProvider` without `instanceId`/`driver`) as
//! JSON; the provider core stamps the instance identity and decodes it.

use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use serde_json::{json, Map, Value};
use zc_contracts::ClaudeSettings;

use crate::catalog::ClaudeModelCatalog;
use crate::home::{make_claude_environment, resolve_claude_executable_path, Env};
use crate::model_options::read_custom_model_entries;
use crate::options::ClaudeQueryOptions;
use crate::protocol::ProcessQuery;
use crate::skills::discover_claude_skills;
use crate::usage_limits::{claude_usage_response_to_limits, make_unavailable_usage_limits, record_claude_usage_response, ScopedLimitNamesRef};

/// `DEFAULT_TIMEOUT_MS`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(4_000);
/// Bedrock initializes slowly; the probe waits this long for `initialize`.
pub const CAPABILITIES_PROBE_TIMEOUT: Duration = Duration::from_millis(25_000);

/// `ClaudeCapabilitiesProbe`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaudeCapabilitiesProbe {
    pub email: Option<String>,
    pub subscription_type: Option<String>,
    pub token_source: Option<String>,
    pub api_provider: Option<String>,
    /// `ServerProviderSlashCommand` JSON.
    pub slash_commands: Vec<Value>,
    /// `{rate_limits_available, rate_limits}` from `get_usage`, `None` when it failed.
    pub usage: Option<Value>,
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value.map(str::trim).filter(|v| !v.is_empty()).map(str::to_string)
}

/// `dedupeSlashCommands`: first occurrence wins (case-insensitive), later ones fill its gaps.
pub fn dedupe_slash_commands(commands: &[Value]) -> Vec<Value> {
    let mut by_name: indexmap::IndexMap<String, Map<String, Value>> = indexmap::IndexMap::new();
    for command in commands {
        let Some(name) = non_empty(command.get("name").and_then(Value::as_str)) else {
            continue;
        };
        let key = name.to_lowercase();
        match by_name.get_mut(&key) {
            None => {
                let mut entry = command.as_object().cloned().unwrap_or_default();
                entry.insert("name".into(), Value::String(name));
                by_name.insert(key, entry);
            }
            Some(existing) => {
                if !crate::js::truthy(existing.get("description")) && crate::js::truthy(command.get("description")) {
                    existing.insert("description".into(), command["description"].clone());
                }
                let has_hint = existing.get("input").and_then(|i| i.get("hint")).is_some_and(|h| crate::js::truthy(Some(h)));
                if let Some(hint) = command
                    .get("input")
                    .and_then(|i| i.get("hint"))
                    .filter(|h| !has_hint && crate::js::truthy(Some(h)))
                {
                    existing.insert("input".into(), json!({ "hint": hint }));
                }
            }
        }
    }
    by_name.into_values().map(Value::Object).collect()
}

/// `parseClaudeInitializationCommands`.
pub fn parse_initialization_commands(commands: Option<&Value>) -> Vec<Value> {
    let parsed: Vec<Value> = commands
        .and_then(Value::as_array)
        .map(|commands| {
            commands
                .iter()
                .filter_map(|command| {
                    let name = non_empty(command.get("name").and_then(Value::as_str))?;
                    let mut entry = Map::new();
                    entry.insert("name".into(), Value::String(name));
                    if let Some(description) = non_empty(command.get("description").and_then(Value::as_str)) {
                        entry.insert("description".into(), Value::String(description));
                    }
                    if let Some(hint) = non_empty(command.get("argumentHint").and_then(Value::as_str)) {
                        entry.insert("input".into(), json!({ "hint": hint }));
                    }
                    Some(Value::Object(entry))
                })
                .collect()
        })
        .unwrap_or_default();
    dedupe_slash_commands(&parsed)
}

/// `buildClaudeCapabilitiesProbeQueryOptions`.
pub fn capabilities_probe_options(executable_path: &str, environment: &Env, cwd: Option<&str>) -> ClaudeQueryOptions {
    let mut env = environment.clone();
    env.insert("ENABLE_CLAUDEAI_MCP_SERVERS".into(), "false".into());
    env.remove("FORCE_CODE_TERMINAL");
    env.insert("CLAUDE_CODE_AUTO_CONNECT_IDE".into(), "0".into());
    env.insert("CLAUDE_CODE_IDE_SKIP_AUTO_INSTALL".into(), "1".into());
    let mut settings = Map::new();
    settings.insert("disableAllHooks".into(), Value::Bool(true));
    ClaudeQueryOptions {
        persist_session: Some(false),
        path_to_claude_code_executable: executable_path.to_string(),
        setting_sources: Some(vec!["user".into(), "project".into(), "local".into()]),
        settings: Some(settings),
        mcp_servers: Some(Map::new()),
        strict_mcp_config: true,
        env,
        cwd: cwd.map(str::to_string),
        ..ClaudeQueryOptions::default()
    }
}

/// `probeClaudeCapabilities`: account, slash commands and usage from a never-prompted session.
pub async fn probe_claude_capabilities(settings: &ClaudeSettings, environment: &Env, cwd: Option<&str>) -> Option<ClaudeCapabilitiesProbe> {
    let claude_env = make_claude_environment(&settings.home_path, environment.clone());
    let executable = resolve_claude_executable_path(&settings.binary_path, &claude_env);
    let options = capabilities_probe_options(&executable, &claude_env, cwd);
    let (query, _messages) = ProcessQuery::spawn(&options, None, None);
    let result = async {
        let init = tokio::time::timeout(CAPABILITIES_PROBE_TIMEOUT, query.initialization_result())
            .await
            .ok()?
            .ok()?;
        let usage = match tokio::time::timeout(DEFAULT_TIMEOUT, query.get_usage()).await {
            Ok(Ok(response)) => Some(json!({
                "rate_limits_available": response.get("rate_limits_available").cloned().unwrap_or(Value::Null),
                "rate_limits": response.get("rate_limits").cloned().unwrap_or(Value::Null),
            })),
            _ => None,
        };
        let account = init.get("account");
        let field = |key: &str| account.and_then(|a| a.get(key)).and_then(Value::as_str).map(str::to_string);
        Some(ClaudeCapabilitiesProbe {
            email: field("email"),
            subscription_type: field("subscriptionType"),
            token_source: field("tokenSource"),
            api_provider: field("apiProvider"),
            slash_commands: parse_initialization_commands(init.get("commands")),
            usage,
        })
    }
    .await;
    let _ = crate::query::ClaudeQueryRuntime::close(&query);
    result
}

fn title_case_words(value: &str) -> String {
    value
        .split(|c: char| c.is_whitespace() || c == '_' || c == '-')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            let first = chars.next().map(|c| c.to_uppercase().collect::<String>()).unwrap_or_default();
            format!("{first}{}", chars.as_str().to_lowercase())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn squash(value: &str) -> String {
    value
        .to_lowercase()
        .chars()
        .filter(|c| !(c.is_whitespace() || *c == '_' || *c == '-'))
        .collect()
}

/// `claudeSubscriptionLabel`.
fn subscription_label(subscription_type: &str) -> String {
    match squash(subscription_type).as_str() {
        "claudemaxsubscription" | "max" | "maxplan" => "Max".into(),
        "claudemax5xsubscription" | "max5" => "Max 5x".into(),
        "claudemax20xsubscription" | "max20" => "Max 20x".into(),
        "claudeenterprisesubscription" | "enterprise" => "Enterprise".into(),
        "claudeteamsubscription" | "team" => "Team".into(),
        "claudeprosubscription" | "pro" => "Pro".into(),
        "claudefreesubscription" | "free" => "Free".into(),
        _ => title_case_words(subscription_type),
    }
}

/// `formatClaudeSubscriptionAuthLabel`.
fn subscription_auth_label(subscription_type: &str) -> String {
    let label = subscription_label(subscription_type);
    let normalized = squash(&label);
    if normalized.starts_with("claude") && normalized.ends_with("subscription") {
        label
    } else if normalized.starts_with("claude") {
        format!("{label} Subscription")
    } else if normalized.ends_with("subscription") {
        format!("Claude {label}")
    } else {
        format!("Claude {label} Subscription")
    }
}

/// `claudeAuthMetadata` ?? `apiProviderAuthMetadata`.
fn auth_metadata(probe: &ClaudeCapabilitiesProbe) -> Option<(String, String)> {
    if probe
        .token_source
        .as_deref()
        .map(squash)
        .is_some_and(|t| matches!(t.as_str(), "apikey" | "anthropicapikey" | "anthropicauthtoken"))
    {
        return Some(("apiKey".into(), "Claude API Key".into()));
    }
    if let Some(subscription) = probe.subscription_type.as_deref().filter(|s| !s.is_empty()) {
        return Some((subscription.to_string(), subscription_auth_label(subscription)));
    }
    (probe.api_provider.as_deref() == Some("bedrock")).then(|| ("bedrock".to_string(), "Amazon Bedrock".to_string()))
}

/// `parseGenericCliVersion`.
pub fn parse_generic_cli_version(output: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\bv?(\d+\.\d+\.\d+)\b").expect("valid regex"));
    re.captures(output).and_then(|c| c.get(1)).map(|m| m.as_str().to_string())
}

/// `providerModelsFromSettings`: built-ins, then custom models (bare slugs get the default
/// capabilities, collisions with a built-in are dropped).
pub fn provider_models_from_settings(built_in: Vec<Value>, custom_models: &Value) -> Vec<Value> {
    let mut seen: std::collections::HashSet<String> = built_in
        .iter()
        .filter_map(|m| m.get("slug").and_then(Value::as_str).map(str::to_string))
        .collect();
    let mut models = built_in;
    for entry in read_custom_model_entries(custom_models) {
        if !seen.insert(entry.slug.clone()) {
            continue;
        }
        models.push(json!({
            "slug": entry.slug,
            "name": entry.name,
            "isCustom": true,
            "capabilities": entry.capabilities.unwrap_or_else(|| json!({ "optionDescriptors": [] })),
        }));
    }
    models
}

/// `ProviderProbeResult`.
struct ProbeResult {
    installed: bool,
    version: Option<String>,
    status: &'static str,
    auth: Value,
    message: Option<String>,
    usage_limits: Option<Value>,
}

/// `buildServerProvider` with the Claude presentation.
fn build_server_provider(enabled: bool, checked_at: &str, models: Vec<Value>, slash_commands: Vec<Value>, skills: Vec<Value>, probe: ProbeResult) -> Value {
    let mut draft = Map::new();
    draft.insert("displayName".into(), Value::String("Claude".into()));
    draft.insert("showInteractionModeToggle".into(), Value::Bool(true));
    draft.insert("reportsContextWindow".into(), Value::Bool(true));
    draft.insert("enabled".into(), Value::Bool(enabled));
    draft.insert("installed".into(), Value::Bool(probe.installed));
    draft.insert("version".into(), probe.version.map(Value::String).unwrap_or(Value::Null));
    draft.insert("status".into(), Value::String(if enabled { probe.status } else { "disabled" }.into()));
    draft.insert("auth".into(), probe.auth);
    draft.insert("checkedAt".into(), Value::String(checked_at.into()));
    if let Some(message) = probe.message.filter(|m| !m.is_empty()) {
        draft.insert("message".into(), Value::String(message));
    }
    draft.insert("models".into(), Value::Array(models));
    draft.insert("slashCommands".into(), Value::Array(slash_commands));
    draft.insert("skills".into(), Value::Array(skills));
    if let Some(limits) = probe.usage_limits {
        draft.insert("usageLimits".into(), limits);
    }
    Value::Object(draft)
}

/// `COMPACT_SLASH_COMMAND`.
pub fn compact_slash_command() -> Value {
    json!({ "name": "compact", "description": "Summarize the conversation and reduce context usage" })
}

/// What `claude --version` produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionProbe {
    Ok { code: i32, stdout: String, stderr: String },
    Missing,
    Failed,
    TimedOut,
}

/// Run `<claude> --version` with the instance environment.
pub async fn run_version_probe(settings: &ClaudeSettings, environment: &Env) -> VersionProbe {
    let claude_env = make_claude_environment(&settings.home_path, environment.clone());
    let mut command = tokio::process::Command::new(&settings.binary_path);
    command
        .arg("--version")
        .env_clear()
        .envs(&claude_env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return VersionProbe::Missing,
        Err(_) => return VersionProbe::Failed,
    };
    match tokio::time::timeout(DEFAULT_TIMEOUT, child.wait_with_output()).await {
        Err(_) => VersionProbe::TimedOut,
        Ok(Err(_)) => VersionProbe::Failed,
        Ok(Ok(output)) => VersionProbe::Ok {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
    }
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The probe's injectable parts.
pub struct StatusProbeDeps<'a> {
    pub environment: &'a Env,
    pub cwd: Option<&'a str>,
    pub catalog: &'a ClaudeModelCatalog,
    pub scoped_limit_names: Option<&'a ScopedLimitNamesRef>,
    pub checked_at: String,
    pub version: BoxFuture<'a, VersionProbe>,
    pub capabilities: Option<BoxFuture<'a, Option<ClaudeCapabilitiesProbe>>>,
    /// Banked resets for a subscription login, given the CLI version.
    pub reset_credits: Option<ResetCreditsProbe<'a>>,
}

/// Reads the banked resets for a CLI version.
pub type ResetCreditsProbe<'a> = Box<dyn FnOnce(String) -> BoxFuture<'a, Option<Value>> + Send + 'a>;

const DISABLED_MESSAGE: &str = "Claude is disabled in zenith code settings.";

/// `checkClaudeProviderStatus`.
pub async fn check_claude_provider_status(settings: &ClaudeSettings, deps: StatusProbeDeps<'_>) -> Value {
    let custom = serde_json::to_value(&settings.custom_models).unwrap_or(Value::Null);
    let checked_at = deps.checked_at.clone();
    let all_models = provider_models_from_settings(deps.catalog.models.iter().map(|m| m.model.clone()).collect(), &custom);
    let unknown_auth = json!({ "status": "unknown" });
    if !settings.enabled {
        return build_server_provider(
            false,
            &checked_at,
            all_models,
            Vec::new(),
            Vec::new(),
            ProbeResult {
                installed: false,
                version: None,
                status: "warning",
                auth: unknown_auth,
                message: Some(DISABLED_MESSAGE.into()),
                usage_limits: None,
            },
        );
    }
    let fail = |installed: bool, version: Option<String>, message: &str| ProbeResult {
        installed,
        version,
        status: "error",
        auth: json!({ "status": "unknown" }),
        message: Some(message.into()),
        usage_limits: None,
    };
    let (code, stdout, stderr) = match deps.version.await {
        VersionProbe::Missing => {
            return build_server_provider(
                true,
                &checked_at,
                all_models,
                Vec::new(),
                Vec::new(),
                fail(false, None, "Claude Agent CLI (`claude`) was not found on PATH."),
            )
        }
        VersionProbe::Failed => {
            return build_server_provider(
                true,
                &checked_at,
                all_models,
                Vec::new(),
                Vec::new(),
                fail(true, None, "Failed to execute Claude Agent CLI health check."),
            )
        }
        VersionProbe::TimedOut => {
            return build_server_provider(
                true,
                &checked_at,
                all_models,
                Vec::new(),
                Vec::new(),
                fail(true, None, "Claude Agent CLI is installed but failed to run. Timed out while running command."),
            )
        }
        VersionProbe::Ok { code, stdout, stderr } => (code, stdout, stderr),
    };
    let version = parse_generic_cli_version(&format!("{stdout}\n{stderr}"));
    if code != 0 {
        return build_server_provider(
            true,
            &checked_at,
            all_models,
            Vec::new(),
            Vec::new(),
            fail(true, version, "Claude Agent CLI is installed but failed to run."),
        );
    }
    let models = provider_models_from_settings(deps.catalog.models_for_version(version.as_deref()), &custom);
    let upgrade_message = deps.catalog.version_upgrade_message(version.as_deref());
    let capabilities = match deps.capabilities {
        Some(probe) => probe.await,
        None => None,
    };
    let skills: Vec<Value> = discover_claude_skills(&settings.home_path, deps.cwd, deps.environment)
        .iter()
        .filter_map(|s| serde_json::to_value(s).ok())
        .collect();
    let mut commands = vec![compact_slash_command()];
    if let Some(capabilities) = &capabilities {
        commands.extend(capabilities.slash_commands.iter().cloned());
    }
    let slash_commands = dedupe_slash_commands(&commands);
    let Some(capabilities) = capabilities else {
        return build_server_provider(
            true,
            &checked_at,
            models,
            slash_commands,
            skills,
            ProbeResult {
                installed: true,
                version,
                status: "warning",
                auth: json!({ "status": "unknown" }),
                message: Some("Could not verify Claude authentication status from initialization result.".into()),
                usage_limits: None,
            },
        );
    };
    let usage_limits = match &capabilities.usage {
        None => make_unavailable_usage_limits(&checked_at, "probeFailed", None),
        Some(response) => match deps.scoped_limit_names {
            Some(names) => record_claude_usage_response(names, response, &checked_at),
            None => claude_usage_response_to_limits(response, &checked_at).0,
        },
    };
    let reset_credits = match (deps.reset_credits, &capabilities.subscription_type, usage_limits.get("unavailable"), &version) {
        (Some(resolve), Some(_), None, Some(version)) => resolve(version.clone()).await,
        _ => None,
    };
    let mut auth = Map::new();
    auth.insert("status".into(), Value::String("authenticated".into()));
    if let Some(email) = capabilities.email.as_deref().filter(|e| !e.is_empty()) {
        auth.insert("email".into(), Value::String(email.into()));
    }
    if let Some((kind, label)) = auth_metadata(&capabilities) {
        auth.insert("type".into(), Value::String(kind));
        auth.insert("label".into(), Value::String(label));
    }
    let mut usage_limits = usage_limits;
    if let (Some(credits), Some(limits)) = (reset_credits, usage_limits.as_object_mut()) {
        limits.insert("resetCredits".into(), credits);
    }
    build_server_provider(
        true,
        &checked_at,
        models,
        slash_commands,
        skills,
        ProbeResult {
            installed: true,
            version,
            status: "ready",
            auth: Value::Object(auth),
            message: upgrade_message,
            usage_limits: Some(usage_limits),
        },
    )
}

/// `makePendingClaudeProvider`: the snapshot before the first probe.
pub fn make_pending_claude_provider(settings: &ClaudeSettings, catalog: &ClaudeModelCatalog, checked_at: &str) -> Value {
    let custom = serde_json::to_value(&settings.custom_models).unwrap_or(Value::Null);
    let models = provider_models_from_settings(catalog.models.iter().map(|m| m.model.clone()).collect(), &custom);
    let message = if settings.enabled {
        "Claude provider status has not been checked in this session yet."
    } else {
        DISABLED_MESSAGE
    };
    build_server_provider(
        settings.enabled,
        checked_at,
        models,
        Vec::new(),
        Vec::new(),
        ProbeResult {
            installed: false,
            version: None,
            status: "warning",
            auth: json!({ "status": "unknown" }),
            message: Some(message.into()),
            usage_limits: None,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(value: Value) -> ClaudeSettings {
        serde_json::from_value(value).unwrap()
    }

    fn deps<'a>(env: &'a Env, catalog: &'a ClaudeModelCatalog, version: VersionProbe, capabilities: Option<ClaudeCapabilitiesProbe>) -> StatusProbeDeps<'a> {
        StatusProbeDeps {
            environment: env,
            cwd: None,
            catalog,
            scoped_limit_names: None,
            checked_at: "2026-03-01T00:00:00.000Z".into(),
            version: Box::pin(async move { version }),
            capabilities: Some(Box::pin(async move { capabilities })),
            reset_credits: None,
        }
    }

    #[tokio::test]
    async fn reports_a_missing_cli_and_a_disabled_instance() {
        let env = Env::new();
        let catalog = ClaudeModelCatalog::default();
        let missing = check_claude_provider_status(&settings(json!({})), deps(&env, &catalog, VersionProbe::Missing, None)).await;
        assert_eq!(missing["installed"], json!(false));
        assert_eq!(missing["status"], json!("error"));
        assert_eq!(missing["message"], json!("Claude Agent CLI (`claude`) was not found on PATH."));
        let disabled = check_claude_provider_status(&settings(json!({"enabled": false})), deps(&env, &catalog, VersionProbe::Missing, None)).await;
        assert_eq!(disabled["status"], json!("disabled"));
        assert_eq!(disabled["message"], json!(DISABLED_MESSAGE));
    }

    #[tokio::test]
    async fn reports_an_authenticated_subscription_with_commands_and_usage() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::new();
        let catalog = ClaudeModelCatalog::default();
        let probe = ClaudeCapabilitiesProbe {
            email: Some("someone@example.test".into()),
            subscription_type: Some("max".into()),
            slash_commands: parse_initialization_commands(Some(&json!([
                {"name": "review", "description": "Review", "argumentHint": "<pr>"},
                {"name": "Review", "description": "dup"},
                {"name": "compact", "description": ""}
            ]))),
            usage: Some(json!({"rate_limits_available": true, "rate_limits": {"five_hour": {"utilization": 10, "resets_at": null}}})),
            ..ClaudeCapabilitiesProbe::default()
        };
        let draft = check_claude_provider_status(
            &settings(json!({"homePath": home.path().to_string_lossy()})),
            deps(
                &env,
                &catalog,
                VersionProbe::Ok {
                    code: 0,
                    stdout: "2.1.276 (Claude Code)".into(),
                    stderr: String::new(),
                },
                Some(probe),
            ),
        )
        .await;
        assert_eq!(draft["status"], json!("ready"));
        assert_eq!(draft["version"], json!("2.1.276"));
        assert_eq!(
            draft["auth"],
            json!({"status": "authenticated", "email": "someone@example.test", "type": "max", "label": "Claude Max Subscription"})
        );
        assert_eq!(
            draft["slashCommands"],
            json!([{"name": "compact", "description": "Summarize the conversation and reduce context usage"}, {"name": "review", "description": "Review", "input": {"hint": "<pr>"}}])
        );
        assert_eq!(draft["usageLimits"]["windows"][0]["id"], json!("five_hour"));
        let decoded: zc_contracts::ServerProvider = serde_json::from_value({
            let mut full = draft.clone();
            full["instanceId"] = json!("claudeAgent");
            full["driver"] = json!("claudeAgent");
            full
        })
        .unwrap();
        assert_eq!(decoded.display_name.as_deref(), Some("Claude"));
    }

    #[test]
    fn labels_subscriptions() {
        assert_eq!(subscription_auth_label("claude_max_subscription"), "Claude Max Subscription");
        assert_eq!(subscription_auth_label("pro"), "Claude Pro Subscription");
        assert_eq!(subscription_auth_label("special plan"), "Claude Special Plan Subscription");
        let api = ClaudeCapabilitiesProbe {
            token_source: Some("ANTHROPIC_API_KEY".into()),
            ..ClaudeCapabilitiesProbe::default()
        };
        assert_eq!(auth_metadata(&api), Some(("apiKey".into(), "Claude API Key".into())));
        let bedrock = ClaudeCapabilitiesProbe {
            api_provider: Some("bedrock".into()),
            ..ClaudeCapabilitiesProbe::default()
        };
        assert_eq!(auth_metadata(&bedrock), Some(("bedrock".into(), "Amazon Bedrock".into())));
    }

    #[test]
    fn builds_probe_options_without_hooks_or_mcp() {
        let options = capabilities_probe_options("claude", &Env::from([("FORCE_CODE_TERMINAL".to_string(), "1".to_string())]), Some("/w"));
        let spec = crate::options::build_spawn_spec(&options);
        assert!(spec.args.contains(&"--no-session-persistence".to_string()));
        assert!(spec.args.contains(&"--strict-mcp-config".to_string()));
        assert!(!spec.args.iter().any(|a| a == "--mcp-config" || a == "--permission-prompt-tool"));
        assert!(!spec.env.contains_key("FORCE_CODE_TERMINAL"));
        assert_eq!(spec.env.get("ENABLE_CLAUDEAI_MCP_SERVERS").map(String::as_str), Some("false"));
    }

    // Ports of `ClaudeCapabilitiesProbe.test.ts`.

    #[test]
    fn isolates_claude_capability_probes_without_dropping_workspace_setting_sources() {
        let environment = Env::from([
            ("HOME".to_string(), "/home/user".to_string()),
            ("ENABLE_CLAUDEAI_MCP_SERVERS".to_string(), "true".to_string()),
            ("FORCE_CODE_TERMINAL".to_string(), "1".to_string()),
        ]);
        let options = capabilities_probe_options("/usr/bin/claude", &environment, Some("/workspace/project"));
        assert_eq!(options.mcp_servers, Some(Map::new()));
        assert!(options.strict_mcp_config);
        assert_eq!(options.cwd.as_deref(), Some("/workspace/project"));
        assert_eq!(
            options.setting_sources,
            Some(vec!["user".to_string(), "project".to_string(), "local".to_string()])
        );
        assert_eq!(options.settings.map(Value::Object), Some(json!({"disableAllHooks": true})));
        assert!(options.allowed_tools.is_empty());
        assert_eq!(options.persist_session, Some(false));
        assert_eq!(options.path_to_claude_code_executable, "/usr/bin/claude");
        assert_eq!(options.env.get("HOME").map(String::as_str), Some("/home/user"));
        assert_eq!(options.env.get("ENABLE_CLAUDEAI_MCP_SERVERS").map(String::as_str), Some("false"));
        assert_eq!(options.env.get("FORCE_CODE_TERMINAL"), None);
        assert_eq!(options.env.get("CLAUDE_CODE_AUTO_CONNECT_IDE").map(String::as_str), Some("0"));
        assert_eq!(options.env.get("CLAUDE_CODE_IDE_SKIP_AUTO_INSTALL").map(String::as_str), Some("1"));
    }

    /// A fake `claude` (run by node): records its argv, cwd and connector env, answers
    /// `initialize`, answers `get_usage` unless `FAKE_PROBE_STALL_USAGE` is set, and marks
    /// `FAKE_PROBE_CLOSED_PATH` when its stdin closes.
    const FAKE_PROBE_CLI: &str = r#"#!/usr/bin/env node
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { createInterface } from "node:readline";
const args = process.argv.slice(2);
const mcpConfigIndex = args.indexOf("--mcp-config");
const rawMcpConfig = mcpConfigIndex >= 0 ? args[mcpConfigIndex + 1] : undefined;
let mcpConfig;
if (rawMcpConfig) {
  const contents = existsSync(rawMcpConfig) ? readFileSync(rawMcpConfig, "utf8") : rawMcpConfig;
  try { mcpConfig = JSON.parse(contents); } catch { mcpConfig = contents; }
}
writeFileSync(process.env.FAKE_PROBE_INVOCATION_PATH, JSON.stringify({
  args,
  cwd: process.cwd(),
  connectorEnv: process.env.ENABLE_CLAUDEAI_MCP_SERVERS,
  mcpConfig,
}));
process.stdin.on("close", () => writeFileSync(process.env.FAKE_PROBE_CLOSED_PATH, "closed"));
const lines = createInterface({ input: process.stdin });
lines.on("line", (line) => {
  const message = JSON.parse(line);
  if (message.type !== "control_request") return;
  const reply = (response) => process.stdout.write(JSON.stringify({
    type: "control_response",
    response: { subtype: "success", request_id: message.request_id, response },
  }) + "\n");
  if (message.request?.subtype === "initialize") {
    reply({
      commands: [{ name: "review", description: "Review changes", argumentHint: "[path]" }],
      agents: [],
      output_style: "default",
      available_output_styles: ["default"],
      models: [],
      account: { email: "dev@example.com", subscriptionType: "pro", tokenSource: "oauth" },
    });
  }
  // The probe follows initialize with get_usage on the same process.
  if (message.request?.subtype === "get_usage" && !process.env.FAKE_PROBE_STALL_USAGE) {
    reply({
      session: {},
      subscription_type: "pro",
      rate_limits_available: true,
      rate_limits: { five_hour: { utilization: 12, resets_at: "2026-07-18T14:39:00Z" } },
      behaviors: null,
    });
  }
});
setInterval(() => {}, 1_000);
"#;

    struct FakeProbe {
        dir: tempfile::TempDir,
        workspace: tempfile::TempDir,
        executable: std::path::PathBuf,
    }

    impl FakeProbe {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let workspace = tempfile::tempdir().unwrap();
            let executable = dir.path().join("fake-claude.mjs");
            std::fs::write(&executable, FAKE_PROBE_CLI).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            Self { dir, workspace, executable }
        }

        fn invocation_path(&self) -> std::path::PathBuf {
            self.dir.path().join("invocation.json")
        }

        fn closed_path(&self) -> std::path::PathBuf {
            self.dir.path().join("closed")
        }

        fn env(&self, stall_usage: bool) -> Env {
            let mut env = Env::from([
                ("PATH".to_string(), std::env::var("PATH").unwrap_or_default()),
                ("FAKE_PROBE_INVOCATION_PATH".to_string(), self.invocation_path().display().to_string()),
                ("FAKE_PROBE_CLOSED_PATH".to_string(), self.closed_path().display().to_string()),
                ("ENABLE_CLAUDEAI_MCP_SERVERS".to_string(), "true".to_string()),
            ]);
            if stall_usage {
                env.insert("FAKE_PROBE_STALL_USAGE".into(), "1".into());
            }
            env
        }

        fn settings(&self) -> ClaudeSettings {
            settings(json!({"binaryPath": self.executable.display().to_string()}))
        }
    }

    fn review_command() -> Vec<Value> {
        vec![json!({"name": "review", "description": "Review changes", "input": {"hint": "[path]"}})]
    }

    #[tokio::test]
    async fn serializes_strict_no_mcp_options_and_still_resolves_account_capabilities() {
        let fake = FakeProbe::new();
        let cwd = fake.workspace.path().display().to_string();
        let capabilities = probe_claude_capabilities(&fake.settings(), &fake.env(false), Some(&cwd)).await;
        assert_eq!(
            capabilities,
            Some(ClaudeCapabilitiesProbe {
                email: Some("dev@example.com".into()),
                subscription_type: Some("pro".into()),
                token_source: Some("oauth".into()),
                api_provider: None,
                slash_commands: review_command(),
                usage: Some(json!({"rate_limits_available": true, "rate_limits": {"five_hour": {"utilization": 12, "resets_at": "2026-07-18T14:39:00Z"}}})),
            })
        );

        let invocation: Value = serde_json::from_str(&std::fs::read_to_string(fake.invocation_path()).unwrap()).unwrap();
        let real_cwd = std::fs::canonicalize(fake.workspace.path()).unwrap();
        assert_eq!(invocation["cwd"], json!(real_cwd.display().to_string()));
        assert_eq!(invocation["connectorEnv"], json!("false"));
        let args: Vec<String> = serde_json::from_value(invocation["args"].clone()).unwrap();
        assert!(args.contains(&"--strict-mcp-config".to_string()));
        assert!(!args.contains(&"--mcp-config".to_string()));
        assert_eq!(invocation.get("mcpConfig"), None);
        assert!(args.contains(&"--setting-sources=user,project,local".to_string()), "{args:?}");
        let settings_flag = args.iter().position(|a| a == "--settings").expect("--settings flag");
        let flag_settings: Value = serde_json::from_str(args.get(settings_flag + 1).map(String::as_str).unwrap_or("{}")).unwrap();
        assert_eq!(flag_settings["disableAllHooks"], json!(true));
    }

    #[tokio::test]
    async fn preserves_initialized_capabilities_when_optional_usage_times_out() {
        let fake = FakeProbe::new();
        let capabilities = probe_claude_capabilities(&fake.settings(), &fake.env(true), None)
            .await
            .expect("initialized capabilities");
        assert_eq!(capabilities.email.as_deref(), Some("dev@example.com"));
        assert_eq!(capabilities.subscription_type.as_deref(), Some("pro"));
        assert_eq!(capabilities.token_source.as_deref(), Some("oauth"));
        assert_eq!(capabilities.slash_commands, review_command());
        assert_eq!(capabilities.usage, None);
        // The probe's session is torn down: the CLI sees its input end.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !fake.closed_path().exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(fake.closed_path().exists(), "the probe did not close its session");
    }
}
