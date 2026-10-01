//! The Codex status probe (`provider/Layers/CodexProvider.ts`): a short-lived `codex app-server`
//! reads the account, models, skills and rate limits, and the result becomes the provider
//! snapshot draft (a `ServerProvider` without `instanceId` / `driver`, in the TS JSON shape).

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::{json, Map, Value};
use zc_codex_protocol as protocol;
use zc_codex_protocol::client_requests;
use zc_contracts::{CodexSettings, CustomModelSetting, ServerProvider, ServerProviderModel, ServerProviderSkill};

use crate::client::{decode_response, request};
use crate::errors::{CodexAppServerError, RequestError};
use crate::launch_args::{codex_app_server_args, resolve_codex_launch_args, Environment};
use crate::model::{codex_model_family, from_json, read_custom_model_entries, PREFERRED_DEFAULT_CODEX_MODELS};
use crate::peer::{CodexPeer, IncomingHandler};
use crate::process::{spawn, ChildHandle, SpawnSpec, FORCE_KILL_AFTER};
use crate::session_runtime::build_codex_initialize_params;
use crate::usage_limits::{
    codex_rate_limits_failure_message, codex_rate_limits_to_limits, make_unavailable_usage_limits, CodexRateLimitSnapshot, CodexResetCreditsSummary,
};

/// `AUTH_PROBE_TIMEOUT_MS`.
pub const AUTH_PROBE_TIMEOUT: Duration = Duration::from_millis(10_000);
/// `RATE_LIMITS_PROBE_TIMEOUT_MS`.
pub const RATE_LIMITS_PROBE_TIMEOUT: Duration = Duration::from_millis(3_000);

const DEFAULT_SERVICE_TIER_ID: &str = "default";

fn reasoning_effort_label(effort: &str) -> String {
    match effort {
        "none" => "None",
        "minimal" => "Minimal",
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "Extra High",
        "max" => "Max",
        "ultra" => "Ultra",
        other => return other.to_owned(),
    }
    .to_owned()
}

/// `codexPlanLabel` (shared with usage-limit sources).
pub fn codex_plan_label(plan_type: Option<&str>) -> Option<&'static str> {
    Some(match plan_type? {
        "free" => "ChatGPT Free Subscription",
        "go" => "ChatGPT Go Subscription",
        "plus" => "ChatGPT Plus Subscription",
        "pro" => "ChatGPT Pro 20x Subscription",
        "prolite" => "ChatGPT Pro 5x Subscription",
        "promax" => "ChatGPT Pro Max Subscription",
        "team" => "ChatGPT Team Subscription",
        "self_serve_business_prolite" | "self_serve_business_usage_based" | "business" => "ChatGPT Business Subscription",
        "ent26" | "enterprise_cbp_automation" | "enterprise_cbp_usage_based" | "enterprise" => "ChatGPT Enterprise Subscription",
        "edu" | "edu_plus" | "edu_pro" => "ChatGPT Edu Subscription",
        "unknown" => "ChatGPT Subscription",
        _ => return None,
    })
}

/// `mapCodexModelCapabilities`: reasoning-effort and service-tier selectors.
pub fn map_codex_model_capabilities(model: &protocol::Model) -> Value {
    let default_effort = if codex_model_family(&model.model) == "gpt-6-astra" {
        "medium".to_owned()
    } else {
        model.default_reasoning_effort.clone()
    };
    let reasoning: Vec<Value> = model
        .supported_reasoning_efforts
        .iter()
        .map(|option| {
            let id = &option.reasoning_effort;
            let mut choice = json!({"id": id, "label": reasoning_effort_label(id)});
            if *id == default_effort {
                choice["isDefault"] = json!(true);
            }
            choice
        })
        .collect();
    let default_reasoning = reasoning.iter().find(|option| option["isDefault"] == true).map(|option| option["id"].clone());
    let tiers: Vec<(String, String, String)> = match model.service_tiers.as_ref().filter(|tiers| !tiers.is_empty()) {
        Some(tiers) => tiers
            .iter()
            .map(|tier| (tier.id.clone(), tier.name.clone(), tier.description.clone()))
            .collect(),
        None => model
            .additional_speed_tiers
            .iter()
            .flatten()
            .map(|id| (id.clone(), if id == "fast" { "Fast".to_owned() } else { id.clone() }, String::new()))
            .collect(),
    };
    let catalog_default = model
        .default_service_tier
        .clone()
        .flatten()
        .filter(|default| tiers.iter().any(|(id, _, _)| id == default));
    let default_tier = catalog_default.unwrap_or_else(|| DEFAULT_SERVICE_TIER_ID.to_owned());
    let mut descriptors = Vec::new();
    if !reasoning.is_empty() {
        let mut descriptor = json!({"id": "reasoningEffort", "label": "Reasoning", "type": "select", "options": reasoning});
        if let Some(current) = default_reasoning {
            descriptor["currentValue"] = current;
        }
        descriptors.push(descriptor);
    }
    if !tiers.is_empty() {
        let mut standard = json!({"id": DEFAULT_SERVICE_TIER_ID, "label": "Standard"});
        if default_tier == DEFAULT_SERVICE_TIER_ID {
            standard["isDefault"] = json!(true);
        }
        let mut options = vec![standard];
        for (id, name, description) in &tiers {
            let description = if id == "ultrafast" {
                "Even faster, more expensive".to_owned()
            } else {
                description.clone()
            };
            let mut option = json!({"id": id, "label": name});
            if !description.is_empty() {
                option["description"] = json!(description);
            }
            if &default_tier == id {
                option["isDefault"] = json!(true);
            }
            options.push(option);
        }
        descriptors.push(json!({"id": "serviceTier", "label": "Service Tier", "type": "select", "options": options, "currentValue": default_tier}));
    }
    json!({ "optionDescriptors": descriptors })
}

/// `toDisplayName`: `gpt-…` → `GPT-…`, a letter after a dash capitalized.
fn to_display_name(display_name: &str) -> String {
    let base = if display_name.len() >= 3 && display_name[..3].eq_ignore_ascii_case("gpt") {
        format!("GPT{}", &display_name[3..])
    } else {
        display_name.to_owned()
    };
    let chars: Vec<char> = base.chars().collect();
    let mut out = String::with_capacity(base.len());
    let mut index = 0;
    while index < chars.len() {
        out.push(chars[index]);
        if chars[index] == '-' && chars.get(index + 1).is_some_and(char::is_ascii_lowercase) {
            out.push(chars[index + 1].to_ascii_uppercase());
            index += 1;
        }
        index += 1;
    }
    out
}

fn parse_model_list(response: &protocol::ModelListResponse) -> Vec<ServerProviderModel> {
    response
        .data
        .iter()
        .map(|model| {
            let mut value = json!({
                "slug": model.model,
                "name": to_display_name(&model.display_name),
                "isCustom": false,
                "capabilities": map_codex_model_capabilities(model),
            });
            if model.is_default {
                value["isDefault"] = json!(true);
            }
            from_json(value)
        })
        .collect()
}

/// `applyPreferredCodexDefaultModel`: our ranking wins when a preferred slug is in the live
/// catalog; otherwise Codex's own default stays.
pub fn apply_preferred_codex_default_model(models: Vec<ServerProviderModel>) -> Vec<ServerProviderModel> {
    let preferred = PREFERRED_DEFAULT_CODEX_MODELS
        .iter()
        .find_map(|slug| models.iter().find(|model| !model.is_custom && codex_model_family(&model.slug) == *slug))
        .map(|model| model.slug.clone());
    let Some(preferred) = preferred else { return models };
    models
        .into_iter()
        .map(|mut model| {
            if model.slug == preferred {
                model.is_default = Some(true);
            } else if model.is_default == Some(true) {
                model.is_default = None;
            }
            model
        })
        .collect()
}

/// `appendCustomCodexModels`: a bare custom slug borrows the first built-in's capabilities.
pub fn append_custom_codex_models(models: Vec<ServerProviderModel>, custom: &[CustomModelSetting]) -> Vec<ServerProviderModel> {
    if custom.is_empty() {
        return models;
    }
    let fallback = models.iter().find_map(|model| model.capabilities.clone());
    let mut seen: Vec<String> = models.iter().map(|model| model.slug.clone()).collect();
    let mut out = models;
    for entry in read_custom_model_entries(custom) {
        if seen.contains(&entry.slug) {
            continue;
        }
        seen.push(entry.slug.clone());
        out.push(ServerProviderModel {
            slug: entry.slug,
            name: entry.name,
            short_name: None,
            sub_provider: None,
            aliases: None,
            badge: None,
            is_custom: true,
            is_default: None,
            is_legacy: None,
            capabilities: entry.capabilities.or_else(|| fallback.clone()),
        });
    }
    out
}

/// `parseCodexSkillsListResponse`: the skills of `cwd`, else of every listed root.
pub fn parse_codex_skills_list_response(response: &protocol::SkillsListResponse, cwd: &str) -> Vec<ServerProviderSkill> {
    let skills: Vec<&protocol::SkillMetadata> = match response.data.iter().find(|entry| entry.cwd == cwd) {
        Some(entry) => entry.skills.iter().collect(),
        None => response.data.iter().flat_map(|entry| entry.skills.iter()).collect(),
    };
    skills
        .into_iter()
        .map(|skill| {
            let interface = skill.interface.clone().flatten();
            let short = skill
                .short_description
                .clone()
                .flatten()
                .or_else(|| interface.as_ref().and_then(|interface| interface.short_description.clone().flatten()));
            let mut value = json!({"name": skill.name, "path": skill.path, "enabled": skill.enabled});
            if !skill.description.is_empty() {
                value["description"] = json!(skill.description);
            }
            let scope = skill.scope.as_str();
            if !scope.is_empty() {
                value["scope"] = json!(scope);
            }
            if let Some(display) = interface
                .as_ref()
                .and_then(|interface| interface.display_name.clone().flatten())
                .filter(|name| !name.is_empty())
            {
                value["displayName"] = json!(display);
            }
            if let Some(short) = short.filter(|short| !short.is_empty()) {
                value["shortDescription"] = json!(short);
            }
            from_json(value)
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The short-lived client

/// Answers unknown server requests with methodNotFound and ignores notifications (a client
/// with no handlers registered).
struct ProbeHandler;

impl IncomingHandler for ProbeHandler {
    fn on_notification(&self, _method: String, _params: Option<Value>) {}
    fn on_request(&self, method: String, _params: Option<Value>) -> BoxFuture<'static, Result<Value, RequestError>> {
        Box::pin(async move { Err(RequestError::method_not_found(&method)) })
    }
}

/// What the probe spawns (`withCodexAppServerClient`): the environment always extends the
/// server's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeInput {
    pub binary_path: String,
    pub home_path: Option<String>,
    pub launch_args: Option<String>,
    pub cwd: String,
    pub environment: Option<Environment>,
}

impl ProbeInput {
    pub fn spawn_spec(&self) -> SpawnSpec {
        let mut env = self.environment.clone().unwrap_or_default();
        if let Some(home) = self.home_path.as_deref().filter(|home| !home.is_empty()) {
            env.insert("CODEX_HOME".into(), zc_core::expand_home_path(home).to_string_lossy().into_owned());
        }
        SpawnSpec {
            command: self.binary_path.clone(),
            args: codex_app_server_args(self.launch_args.as_deref()),
            cwd: self.cwd.clone(),
            env,
            extend_env: true,
        }
    }
}

/// A connected, initialized short-lived app-server; killed on drop.
pub struct CodexAppServerClient {
    pub peer: CodexPeer,
    pub initialize: protocol::InitializeResponse,
    child: ChildHandle,
    drain: tokio::task::AbortHandle,
}

impl CodexAppServerClient {
    /// Spawns, runs the `initialize` handshake and sends `initialized`.
    pub async fn connect(input: &ProbeInput) -> Result<Self, CodexAppServerError> {
        let child = spawn(&input.spawn_spec())?;
        let termination = child.handle.clone();
        let peer = CodexPeer::start(
            child.stdout,
            child.stdin,
            Arc::new(ProbeHandler),
            Some(Box::pin(async move { termination.termination_error().await })),
        );
        // Drain stderr so large diagnostics cannot block the protocol.
        let mut stderr = child.stderr;
        let drain = tokio::spawn(async move {
            let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
        })
        .abort_handle();
        let client = Self {
            peer,
            initialize: protocol::InitializeResponse::default(),
            child: child.handle,
            drain,
        };
        let initialize = client.peer.request("initialize", Some(build_codex_initialize_params())).await?;
        let initialize = decode_response::<protocol::InitializeResponse>("initialize", initialize)?;
        client.peer.notify("initialized", None)?;
        Ok(Self { initialize, ..client })
    }

    /// Stops the process (SIGTERM, SIGKILL after 2 s).
    pub async fn close(self) {
        self.peer.shutdown();
        self.drain.abort();
        self.child.kill(FORCE_KILL_AFTER).await;
    }
}

/// `CodexAppServerProviderSnapshot`.
#[derive(Debug, Clone)]
pub struct CodexProbeSnapshot {
    pub account: protocol::GetAccountResponse,
    /// `Ok((snapshot, byLimitId, resetCredits))` or the failure message; `None` when skipped.
    pub rate_limits: Option<Result<RateLimitsRead, String>>,
    pub version: Option<String>,
    pub models: Vec<ServerProviderModel>,
    pub skills: Vec<ServerProviderSkill>,
}

#[derive(Debug, Clone)]
pub struct RateLimitsRead {
    pub snapshot: CodexRateLimitSnapshot,
    pub by_limit_id: Option<Map<String, Value>>,
    pub reset_credits: Option<CodexResetCreditsSummary>,
}

/// The version after the first `/` of `userAgent`, up to a space.
pub fn version_from_user_agent(user_agent: &str) -> Option<String> {
    let rest = &user_agent[user_agent.find('/')? + 1..];
    let version: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
    (!version.is_empty()).then_some(version)
}

async fn request_all_models(client: &CodexAppServerClient) -> Result<Vec<ServerProviderModel>, CodexAppServerError> {
    let mut models = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let params: protocol::ModelListParams = from_json(match &cursor {
            Some(cursor) => json!({ "cursor": cursor }),
            None => json!({}),
        });
        let response = request::<client_requests::ModelList>(&client.peer, &params).await?;
        models.extend(parse_model_list(&response));
        cursor = response.next_cursor.flatten().filter(|cursor| !cursor.is_empty());
        if cursor.is_none() {
            break;
        }
    }
    Ok(models)
}

async fn read_rate_limits(client: &CodexAppServerClient) -> Result<RateLimitsRead, String> {
    let read = request::<client_requests::AccountRateLimitsRead>(&client.peer, &None);
    match tokio::time::timeout(RATE_LIMITS_PROBE_TIMEOUT, read).await {
        Err(_) => Err("Codex did not answer the usage request.".to_owned()),
        Ok(Err(error)) => {
            tracing::debug!(%error, "Codex rate-limit read failed.");
            Err(codex_rate_limits_failure_message(&error))
        }
        Ok(Ok(response)) => Ok(RateLimitsRead {
            snapshot: serde_json::to_value(&response.rate_limits)
                .ok()
                .and_then(|value| CodexRateLimitSnapshot::from_value(&value))
                .unwrap_or_default(),
            by_limit_id: response
                .rate_limits_by_limit_id
                .clone()
                .flatten()
                .and_then(|by_id| serde_json::to_value(by_id).ok())
                .and_then(|value| value.as_object().cloned()),
            reset_credits: response
                .rate_limit_reset_credits
                .clone()
                .flatten()
                .and_then(|credits| serde_json::to_value(credits).ok())
                .and_then(|value| serde_json::from_value(value).ok()),
        }),
    }
}

/// `probeCodexAppServerProvider`.
pub async fn probe_codex_app_server_provider(
    input: &ProbeInput,
    custom_models: &[CustomModelSetting],
    skip_native_usage: bool,
) -> Result<CodexProbeSnapshot, CodexAppServerError> {
    let client = CodexAppServerClient::connect(input).await?;
    let result = async {
        let version = version_from_user_agent(&client.initialize.user_agent);
        let account_params: protocol::GetAccountParams = from_json(json!({}));
        let account = request::<client_requests::AccountRead>(&client.peer, &account_params).await?;
        if account.account.clone().flatten().is_none() && account.requires_openai_auth {
            return Ok(CodexProbeSnapshot {
                account,
                rate_limits: None,
                version,
                models: append_custom_codex_models(Vec::new(), custom_models),
                skills: Vec::new(),
            });
        }
        let skills_params: protocol::SkillsListParams = from_json(json!({ "cwds": [input.cwd] }));
        let (skills, models, rate_limits) = tokio::join!(
            request::<client_requests::SkillsList>(&client.peer, &skills_params),
            request_all_models(&client),
            async {
                if skip_native_usage {
                    None
                } else {
                    Some(read_rate_limits(&client).await)
                }
            }
        );
        Ok(CodexProbeSnapshot {
            account,
            rate_limits,
            version,
            models: apply_preferred_codex_default_model(append_custom_codex_models(models?, custom_models)),
            skills: parse_codex_skills_list_response(&skills?, &input.cwd),
        })
    }
    .await;
    client.close().await;
    result
}

/// `probeCodexSkillsForCwd`.
pub async fn probe_codex_skills_for_cwd(input: &ProbeInput) -> Result<Vec<ServerProviderSkill>, CodexAppServerError> {
    let client = CodexAppServerClient::connect(input).await?;
    let params: protocol::SkillsListParams = from_json(json!({ "cwds": [input.cwd] }));
    let result = request::<client_requests::SkillsList>(&client.peer, &params).await;
    client.close().await;
    Ok(parse_codex_skills_list_response(&result?, &input.cwd))
}

/// `account/rateLimitResetCredit/consume` on a short-lived client; the outcome string.
pub async fn consume_reset_credit(input: &ProbeInput, idempotency_key: &str) -> Result<String, CodexAppServerError> {
    let client = CodexAppServerClient::connect(input).await?;
    let params: protocol::ConsumeAccountRateLimitResetCreditParams = from_json(json!({ "idempotencyKey": idempotency_key }));
    let result = request::<client_requests::AccountRateLimitResetCreditConsume>(&client.peer, &params).await;
    client.close().await;
    Ok(result?.outcome.as_str().to_owned())
}

// ---------------------------------------------------------------------------------------------
// The snapshot draft

/// `CODEX_PRESENTATION`.
const DISPLAY_NAME: &str = "Codex";

/// `ProviderProbeResult`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeResult {
    pub installed: bool,
    pub version: Option<String>,
    pub status: &'static str,
    pub auth: Value,
    pub message: Option<String>,
    pub usage_limits: Option<Value>,
}

/// `buildServerProvider` with the Codex presentation (no `driver`: no version advisory here).
pub fn build_server_provider_draft(
    enabled: bool,
    checked_at: &str,
    models: &[ServerProviderModel],
    slash_commands: Value,
    skills: &[ServerProviderSkill],
    probe: &ProbeResult,
) -> Value {
    let mut draft = json!({
        "displayName": DISPLAY_NAME,
        "showInteractionModeToggle": true,
        "reportsContextWindow": true,
        "enabled": enabled,
        "installed": probe.installed,
        "version": probe.version,
        "status": if enabled { probe.status } else { "disabled" },
        "auth": probe.auth,
        "checkedAt": checked_at,
    });
    if let Some(message) = probe.message.as_ref().filter(|message| !message.is_empty()) {
        draft["message"] = json!(message);
    }
    draft["models"] = serde_json::to_value(models).expect("models encode");
    draft["slashCommands"] = slash_commands;
    draft["skills"] = serde_json::to_value(skills).expect("skills encode");
    if let Some(limits) = &probe.usage_limits {
        draft["usageLimits"] = limits.clone();
    }
    draft
}

fn empty_models(settings: &CodexSettings) -> Vec<ServerProviderModel> {
    append_custom_codex_models(Vec::new(), &settings.custom_models)
}

fn not_checked(settings: &CodexSettings, checked_at: &str, message: &str) -> Value {
    build_server_provider_draft(
        settings.enabled,
        checked_at,
        &empty_models(settings),
        json!([]),
        &[],
        &ProbeResult {
            installed: false,
            version: None,
            status: "warning",
            auth: json!({"status": "unknown"}),
            message: Some(message.to_owned()),
            usage_limits: None,
        },
    )
}

/// `makePendingCodexProvider`.
pub fn make_pending_codex_provider(settings: &CodexSettings, checked_at: &str) -> Value {
    if !settings.enabled {
        return not_checked(settings, checked_at, &format!("Codex is disabled in {} settings.", crate::BRAND_NAME));
    }
    not_checked(settings, checked_at, "Codex provider status has not been checked in this session yet.")
}

fn account_probe_status(account: &protocol::GetAccountResponse) -> (&'static str, Value, Option<String>) {
    let current = account
        .account
        .clone()
        .flatten()
        .filter(|account| !matches!(account, protocol::Account::Unknown(_)));
    if let Some(current) = &current {
        let mut auth = json!({"status": "authenticated"});
        let kind = current.tag().unwrap_or_default();
        if !kind.is_empty() {
            auth["type"] = json!(kind);
        }
        let label = match current {
            protocol::Account::ApiKey(_) => Some("OpenAI API Key"),
            protocol::Account::AmazonBedrock(_) => Some("Amazon Bedrock"),
            protocol::Account::Chatgpt(chatgpt) => codex_plan_label(Some(chatgpt.plan_type.as_str())),
            protocol::Account::Unknown(_) => None,
        };
        if let Some(label) = label {
            auth["label"] = json!(label);
        }
        if let protocol::Account::Chatgpt(chatgpt) = current {
            if let Some(email) = chatgpt.email.as_ref().filter(|email| !email.is_empty()) {
                auth["email"] = json!(email);
            }
        }
        return ("ready", auth, None);
    }
    if account.requires_openai_auth {
        return (
            "error",
            json!({"status": "unauthenticated"}),
            Some("Codex CLI is not authenticated. Run `codex login` and try again.".to_owned()),
        );
    }
    ("ready", json!({"status": "unknown"}), None)
}

/// The probe, as a function so tests and managed mode can swap it.
pub type ProbeFn = Arc<dyn Fn(ProbeInput, Vec<CustomModelSetting>, bool) -> BoxFuture<'static, Result<CodexProbeSnapshot, CodexAppServerError>> + Send + Sync>;

pub fn default_probe() -> ProbeFn {
    Arc::new(|input, custom_models, skip| Box::pin(async move { probe_codex_app_server_provider(&input, &custom_models, skip).await }))
}

/// What `checkCodexProviderStatus` hands the probe (`environment` defaults to the server's).
pub fn status_probe_input(settings: &CodexSettings, environment: Option<&Environment>, cwd: &str) -> ProbeInput {
    let resolved_environment = environment.cloned().unwrap_or_else(crate::adapter::process_environment);
    ProbeInput {
        binary_path: settings.binary_path.clone(),
        home_path: Some(settings.home_path.clone()).filter(|home| !home.is_empty()),
        launch_args: Some(resolve_codex_launch_args(Some(&settings.launch_args), &resolved_environment)),
        cwd: cwd.to_owned(),
        environment: Some(resolved_environment),
    }
}

/// `checkCodexProviderStatus`: probe (10 s) and build the snapshot draft.
pub async fn check_codex_provider_status(
    settings: &CodexSettings,
    probe: Option<ProbeFn>,
    environment: Option<&Environment>,
    managed_auth: Option<Value>,
    cwd: &str,
    checked_at: &str,
) -> Value {
    if !settings.enabled {
        return not_checked(settings, checked_at, &format!("Codex is disabled in {} settings.", crate::BRAND_NAME));
    }
    let input = status_probe_input(settings, environment, cwd);
    let probe = probe.unwrap_or_else(default_probe);
    let outcome = tokio::time::timeout(AUTH_PROBE_TIMEOUT, probe(input, settings.custom_models.clone(), managed_auth.is_some())).await;
    let failure = |installed: bool, message: String| {
        build_server_provider_draft(
            settings.enabled,
            checked_at,
            &empty_models(settings),
            json!([]),
            &[],
            &ProbeResult {
                installed,
                version: None,
                status: "error",
                auth: json!({"status": "unknown"}),
                message: Some(message),
                usage_limits: None,
            },
        )
    };
    let snapshot = match outcome {
        Err(_) => return failure(true, "Timed out while checking Codex app-server provider status.".to_owned()),
        Ok(Err(error)) => {
            let installed = !matches!(error, CodexAppServerError::Spawn { .. });
            let message = if installed {
                format!("Codex app-server provider probe failed: {error}.")
            } else {
                format!(
                    "Could not start Codex CLI (`{}`). Check Settings → Providers → Codex → Binary path on the server.{}",
                    settings.binary_path,
                    if settings.binary_path == "codex" {
                        " Installing ChatGPT or Codex desktop may not add codex to PATH."
                    } else {
                        " Make sure the configured executable exists and can be run."
                    }
                )
            };
            return failure(installed, message);
        }
        Ok(Ok(snapshot)) => snapshot,
    };
    let (status, auth, message) = match &managed_auth {
        Some(auth) => ("ready", auth.clone(), None),
        None => account_probe_status(&snapshot.account),
    };
    let is_api_key = matches!(snapshot.account.account.clone().flatten(), Some(protocol::Account::ApiKey(_)));
    let usage_limits = if is_api_key {
        make_unavailable_usage_limits(checked_at, "unsupported", None)
    } else {
        match &snapshot.rate_limits {
            None => make_unavailable_usage_limits(checked_at, "probeFailed", None),
            Some(Err(message)) => make_unavailable_usage_limits(checked_at, "probeFailed", Some(message)),
            Some(Ok(read)) => codex_rate_limits_to_limits(&read.snapshot, read.by_limit_id.as_ref(), read.reset_credits.as_ref(), checked_at),
        }
    };
    build_server_provider_draft(
        settings.enabled,
        checked_at,
        &snapshot.models,
        json!([
            {"name": "compact", "description": "Summarize the conversation and reduce context usage"},
            {"name": "feedback", "description": "Send this thread and Codex logs to OpenAI", "input": {"hint": "Describe the issue (optional)"}},
        ]),
        &snapshot.skills,
        &ProbeResult {
            installed: true,
            version: snapshot.version.clone(),
            status,
            auth,
            message,
            usage_limits: if managed_auth.is_some() { None } else { Some(usage_limits) },
        },
    )
}

/// A draft stamped with its identity (the rest of `withInstanceIdentity` is WP-12's).
pub fn draft_into_server_provider(mut draft: Value, instance_id: &str) -> Result<ServerProvider, serde_json::Error> {
    draft["instanceId"] = json!(instance_id);
    draft["driver"] = json!(crate::DRIVER_KIND);
    serde_json::from_value(draft)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(value: Value) -> protocol::Model {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn maps_current_model_capability_fields() {
        let capabilities = map_codex_model_capabilities(&model(json!({
            "additionalSpeedTiers": [], "defaultReasoningEffort": "super-high", "description": "Test model", "displayName": "GPT Test",
            "hidden": false, "id": "gpt-test", "isDefault": true, "model": "gpt-test", "defaultServiceTier": "flex",
            "serviceTiers": [{"id": "priority", "name": "Fast", "description": "Lower latency responses."}, {"id": "flex", "name": "Flex", "description": "Lower-cost asynchronous routing."}],
            "supportedReasoningEfforts": [{"description": "Maximum reasoning", "reasoningEffort": "super-high"}]
        })));
        assert_eq!(
            capabilities["optionDescriptors"],
            json!([
                {"id": "reasoningEffort", "label": "Reasoning", "type": "select", "options": [{"id": "super-high", "label": "super-high", "isDefault": true}], "currentValue": "super-high"},
                {"id": "serviceTier", "label": "Service Tier", "type": "select", "options": [
                    {"id": "default", "label": "Standard"},
                    {"id": "priority", "label": "Fast", "description": "Lower latency responses."},
                    {"id": "flex", "label": "Flex", "description": "Lower-cost asynchronous routing.", "isDefault": true}
                ], "currentValue": "flex"}
            ])
        );
    }

    #[test]
    fn standard_routing_without_a_catalog_default_tier() {
        let capabilities = map_codex_model_capabilities(&model(json!({
            "additionalSpeedTiers": ["fast"], "defaultReasoningEffort": "medium", "defaultServiceTier": null, "description": "Test model",
            "displayName": "GPT Test", "hidden": false, "id": "gpt-test", "isDefault": true, "model": "gpt-test",
            "serviceTiers": [{"id": "priority", "name": "Fast", "description": "1.5x speed, increased usage"}, {"id": "ultrafast", "name": "Ultrafast", "description": "The fastest available responses for latency-sensitive work."}],
            "supportedReasoningEfforts": []
        })));
        assert_eq!(
            capabilities["optionDescriptors"],
            json!([{"id": "serviceTier", "label": "Service Tier", "type": "select", "options": [
                {"id": "default", "label": "Standard", "isDefault": true},
                {"id": "priority", "label": "Fast", "description": "1.5x speed, increased usage"},
                {"id": "ultrafast", "label": "Ultrafast", "description": "Even faster, more expensive"}
            ], "currentValue": "default"}])
        );
    }

    fn models(value: Value) -> Vec<ServerProviderModel> {
        from_json(value)
    }

    fn defaults(models: &[ServerProviderModel]) -> Vec<String> {
        models
            .iter()
            .filter(|model| model.is_default == Some(true))
            .map(|model| model.slug.clone())
            .collect()
    }

    #[test]
    fn preferred_default_model() {
        let ranked = apply_preferred_codex_default_model(models(json!([
            {"slug": "gpt-5.6-terra", "name": "GPT-5.6-Terra", "isCustom": false, "capabilities": null},
            {"slug": "gpt-5.4", "name": "GPT-5.4", "isCustom": false, "isDefault": true, "capabilities": null}
        ])));
        assert_eq!(ranked.iter().map(|model| model.is_default).collect::<Vec<_>>(), vec![Some(true), None]);
        let sol = apply_preferred_codex_default_model(models(json!([
            {"slug": "gpt-5.6-terra", "name": "T", "isCustom": false, "capabilities": null},
            {"slug": "gpt-5.6-sol", "name": "S", "isCustom": false, "capabilities": null}
        ])));
        assert_eq!(defaults(&sol), vec!["gpt-5.6-sol"]);
        let qualified = apply_preferred_codex_default_model(models(json!([
            {"slug": "openai.gpt-5.6-luna", "name": "Luna", "isCustom": false, "isDefault": true, "capabilities": null},
            {"slug": "openai.gpt-5.6-sol", "name": "Sol", "isCustom": false, "capabilities": null}
        ])));
        assert_eq!(defaults(&qualified), vec!["openai.gpt-5.6-sol"]);
        let none = apply_preferred_codex_default_model(models(json!([
            {"slug": "gpt-5.5", "name": "GPT-5.5", "isCustom": false, "capabilities": null},
            {"slug": "gpt-5.4", "name": "GPT-5.4", "isCustom": false, "isDefault": true, "capabilities": null}
        ])));
        assert_eq!(defaults(&none), vec!["gpt-5.4"]);
        let custom = apply_preferred_codex_default_model(models(json!([
            {"slug": "gpt-5.6-sol", "name": "gpt-5.6-sol", "isCustom": true, "capabilities": null},
            {"slug": "gpt-5.4", "name": "GPT-5.4", "isCustom": false, "isDefault": true, "capabilities": null}
        ])));
        assert_eq!(defaults(&custom), vec!["gpt-5.4"]);
    }

    #[test]
    fn display_names_and_versions() {
        assert_eq!(to_display_name("gpt-5.3-codex"), "GPT-5.3-Codex");
        assert_eq!(to_display_name("GPT Test"), "GPT Test");
        assert_eq!(version_from_user_agent("codex_cli_rs/0.156.1 (Mac OS 15; arm64)").as_deref(), Some("0.156.1"));
        assert_eq!(version_from_user_agent("no-version"), None);
        assert_eq!(codex_plan_label(Some("promax")), Some("ChatGPT Pro Max Subscription"));
        assert_eq!(codex_plan_label(Some("something-new")), None);
    }
}
