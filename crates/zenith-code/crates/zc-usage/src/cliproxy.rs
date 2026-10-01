//! A CLIProxyAPI hub's built-in management API (`cliproxyApi.ts`): the accounts it pools,
//! their Codex/Claude quota windows and Codex reset credits, and credit redemption.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::FutureExt;
use futures::stream::{self, StreamExt};
use serde_json::{json, Map, Value};
use sha1::{Digest, Sha1};

use crate::json::{self, J};
use crate::time::{date_parse, iso_from_millis};

const CODEX_BASE: &str = "https://chatgpt.com/backend-api/wham";
const MANAGEMENT_TIMEOUT: Duration = Duration::from_secs(15);

fn credit_url() -> String {
    format!("{CODEX_BASE}/rate-limit-reset-credits")
}

/// `UsageLimitSourceError`: a bounded, client-safe detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceError(pub String);

impl SourceError {
    fn new(detail: &str) -> Self {
        Self(detail.to_owned())
    }

    /// The encoded tagged error.
    pub fn to_value(&self) -> Value {
        json!({"_tag": "UsageLimitSourceError", "detail": self.0})
    }
}

/// A hub request failure: one of ours, or a response that did not decode (a schema error).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Failure {
    Source(SourceError),
    Schema,
}

impl From<SourceError> for Failure {
    fn from(error: SourceError) -> Self {
        Failure::Source(error)
    }
}

/// An HTTP reply.
#[derive(Debug, Clone)]
pub struct HubReply {
    pub status: u16,
    pub body: Vec<u8>,
}

/// The hub transport (Effect `HttpClient` in TS; a fake in tests).
#[async_trait]
pub trait HubHttp: Send + Sync {
    /// GET (no body) or POST a JSON body; `Err` is a network failure or the timeout.
    async fn send(&self, url: &str, authorization: &str, json_body: Option<String>, timeout: Duration) -> Result<HubReply, String>;
}

/// [`HubHttp`] over reqwest.
#[derive(Clone, Default)]
pub struct ReqwestHubHttp {
    client: reqwest::Client,
}

#[async_trait]
impl HubHttp for ReqwestHubHttp {
    async fn send(&self, url: &str, authorization: &str, json_body: Option<String>, timeout: Duration) -> Result<HubReply, String> {
        let request = match json_body {
            None => self.client.get(url),
            Some(body) => self.client.post(url).header("content-type", "application/json").body(body),
        };
        let response = request
            .header("authorization", authorization)
            .timeout(timeout)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status().as_u16();
        let body = response.bytes().await.map_err(|error| error.to_string())?;
        Ok(HubReply { status, body: body.to_vec() })
    }
}

/// One `usageLimitSources` entry (`UsageLimitSourceConfig`).
#[derive(Debug, Clone, PartialEq)]
pub struct SourceConfig {
    pub kind: String,
    pub label: Option<String>,
    pub url: String,
    pub management_key: String,
    pub enabled: bool,
}

impl SourceConfig {
    /// From the decoded settings value.
    pub fn from_value(value: &Value) -> Option<Self> {
        Some(Self {
            kind: value.get("kind")?.as_str()?.to_owned(),
            label: value.get("label").and_then(Value::as_str).map(str::to_owned),
            url: value.get("url")?.as_str()?.to_owned(),
            management_key: value.get("managementKey").and_then(Value::as_str).unwrap_or("").to_owned(),
            enabled: value.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        })
    }
}

/// One pooled account (`AuthFile`, decoded strictly).
#[derive(Debug, Clone, PartialEq)]
struct AuthFile {
    id: String,
    auth_index: String,
    provider: String,
    email: Option<String>,
    disabled: Option<bool>,
    chatgpt_account_id: Option<String>,
    chatgpt_plan_type: Option<String>,
}

/// An optional key of an Effect `Schema.optional(X)`: absent is fine, any other type fails.
fn optional<'a, T>(object: &'a Map<String, Value>, key: &str, read: impl Fn(&'a Value) -> Option<T>) -> Result<Option<T>, Failure> {
    match object.get(key) {
        None => Ok(None),
        Some(value) => read(value).map(Some).ok_or(Failure::Schema),
    }
}

fn decode_auth_files(value: &Value) -> Result<Vec<AuthFile>, Failure> {
    let files = value.get("files").and_then(Value::as_array).ok_or(Failure::Schema)?;
    files
        .iter()
        .map(|file| {
            let file = file.as_object().ok_or(Failure::Schema)?;
            let string = |key: &str| file.get(key).and_then(Value::as_str).map(str::to_owned).ok_or(Failure::Schema);
            let id_token = optional(file, "id_token", Value::as_object)?;
            let (chatgpt_account_id, chatgpt_plan_type) = match id_token {
                None => (None, None),
                Some(token) => (
                    optional(token, "chatgpt_account_id", |value| value.as_str().map(str::to_owned))?,
                    optional(token, "chatgpt_plan_type", |value| value.as_str().map(str::to_owned))?,
                ),
            };
            Ok(AuthFile {
                id: string("id")?,
                auth_index: string("auth_index")?,
                provider: string("provider")?,
                email: optional(file, "email", |value| value.as_str().map(str::to_owned))?,
                disabled: optional(file, "disabled", Value::as_bool)?,
                chatgpt_account_id,
                chatgpt_plan_type,
            })
        })
        .collect()
}

/// `creditRedeemRequestId`: a UUIDv5 per account and credit, so retries (from any T3
/// environment) redeem once.
pub fn credit_redeem_request_id(account_id: &str, credit_id: &str) -> String {
    let namespace: [u8; 16] = [0x6f, 0x1c, 0x2a, 0x9e, 0x2d, 0x4b, 0x4c, 0x1e, 0x9a, 0x7f, 0x3b, 0x8d, 0x5e, 0x0c, 0x1a, 0x42];
    let mut hasher = Sha1::new();
    hasher.update(namespace);
    hasher.update(format!("{account_id}:{credit_id}").as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

/// The hub client.
#[derive(Clone)]
pub struct CliproxyApi {
    http: Arc<dyn HubHttp>,
    /// Clock for `checkedAt` and credit expiry (ms).
    now_ms: Arc<dyn Fn() -> f64 + Send + Sync>,
}

impl CliproxyApi {
    pub fn new(http: Arc<dyn HubHttp>, now_ms: Arc<dyn Fn() -> f64 + Send + Sync>) -> Self {
        Self { http, now_ms }
    }

    fn checked_at(&self) -> String {
        iso_from_millis((self.now_ms)()).unwrap_or_default()
    }

    async fn management(&self, config: &SourceConfig, path: &str, body: Option<Value>) -> Result<J, SourceError> {
        let url = url::Url::parse(&config.url)
            .and_then(|base| base.join(&format!("/v0/management/{path}")))
            .map_err(|_| SourceError::new("The hub URL is not valid."))?;
        let failed = || SourceError::new("The hub management request failed.");
        let reply = self
            .http
            .send(
                url.as_str(),
                &format!("Bearer {}", config.management_key),
                body.map(|body| body.to_string()),
                MANAGEMENT_TIMEOUT,
            )
            .await
            .map_err(|_| failed())?;
        if !(200..300).contains(&reply.status) {
            return Err(failed());
        }
        json::parse(&reply.body).ok_or_else(failed)
    }

    async fn auth_files(&self, config: &SourceConfig) -> Result<Vec<AuthFile>, Failure> {
        let response = self.management(config, "auth-files", None).await?;
        decode_auth_files(&json::to_value(&response))
    }

    async fn api_call(&self, config: &SourceConfig, account: &AuthFile, url: &str, data: Option<Value>) -> Result<String, Failure> {
        let mut header = Map::new();
        if account.provider == "codex" {
            header.insert("Authorization".into(), json!("Bearer $TOKEN$"));
            header.insert("Content-Type".into(), json!("application/json"));
            header.insert("OpenAI-Beta".into(), json!("codex-1"));
            header.insert("Originator".into(), json!("Codex Desktop"));
            if let Some(account_id) = account.chatgpt_account_id.as_deref().filter(|id| !id.is_empty()) {
                header.insert("Chatgpt-Account-Id".into(), json!(account_id));
            }
        } else {
            header.insert("Authorization".into(), json!("Bearer $TOKEN$"));
            header.insert("anthropic-beta".into(), json!("oauth-2025-04-20"));
        }
        let mut body = Map::new();
        body.insert("auth_index".into(), json!(account.auth_index));
        body.insert("method".into(), json!(if data.is_none() { "GET" } else { "POST" }));
        body.insert("url".into(), json!(url));
        body.insert("header".into(), Value::Object(header));
        if let Some(data) = data {
            body.insert("data".into(), json!(data.to_string()));
        }
        let raw = self.management(config, "api-call", Some(Value::Object(body))).await?;
        let status = raw.get("status_code").and_then(J::as_num).ok_or(Failure::Schema)?;
        let body = raw.get("body").and_then(J::as_str).ok_or(Failure::Schema)?;
        if !(200.0..300.0).contains(&status) {
            return Err(SourceError(format!("The provider refused the hub request (HTTP {}).", json::number_to_string(status))).into());
        }
        Ok(body.to_owned())
    }

    /// Available Codex reset credits, earliest expiry first.
    async fn credits(&self, config: &SourceConfig, account: &AuthFile) -> Result<Vec<(String, String)>, Failure> {
        let body = self.api_call(config, account, &credit_url(), None).await?;
        let response = json::parse(body.as_bytes()).ok_or(Failure::Schema)?;
        let credits = match response.get("credits") {
            Some(J::Arr(credits)) => credits,
            _ => return Err(Failure::Schema),
        };
        let mut decoded = Vec::new();
        for credit in credits {
            if !matches!(credit, J::Obj(_)) {
                return Err(Failure::Schema);
            }
            let field = |key: &str| credit.get(key).and_then(J::as_str).map(str::to_owned).ok_or(Failure::Schema);
            decoded.push((field("id")?, field("status")?, field("reset_type")?, field("expires_at")?));
        }
        let now = (self.now_ms)();
        let mut available: Vec<(String, String, f64)> = decoded
            .into_iter()
            .filter_map(|(id, status, reset_type, expires_at)| {
                let expires = date_parse(&expires_at).unwrap_or(f64::NAN);
                (reset_type == "codex_rate_limits" && status == "available" && expires > now).then_some((id, expires_at, expires))
            })
            .collect();
        available.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
        Ok(available.into_iter().map(|(id, expires_at, _)| (id, expires_at)).collect())
    }

    async fn read_account(&self, config: &SourceConfig, account: &AuthFile) -> Value {
        let checked_at = self.checked_at();
        let mut base = Map::new();
        base.insert("id".into(), json!(account.id));
        base.insert("driver".into(), json!(if account.provider == "codex" { "codex" } else { "claudeAgent" }));
        if let Some(email) = account.email.as_deref().filter(|email| !email.is_empty()) {
            base.insert("email".into(), json!(email));
        }
        let read = async {
            if account.provider == "claude" {
                let body = self.api_call(config, account, "https://api.anthropic.com/api/oauth/usage", None).await?;
                let usage = decode_claude_usage(&body)?;
                let response = json!({
                    "rate_limits_available": true,
                    "rate_limits": {
                        "five_hour": usage.five_hour,
                        "seven_day": usage.seven_day,
                        "model_scoped": usage.model_scoped,
                    },
                });
                let (limits, _) = zc_provider_claude::usage_limits::claude_usage_response_to_limits(&response, &checked_at);
                let mut account_value = base.clone();
                account_value.insert("plan".into(), json!("Claude Subscription"));
                account_value.insert("usageLimits".into(), limits);
                return Ok::<Value, Failure>(Value::Object(account_value));
            }
            let body = self.api_call(config, account, &format!("{CODEX_BASE}/usage"), None).await?;
            let usage = decode_codex_usage(&body)?;
            // A credits outage must not hide the quota windows that did arrive.
            let available = self.credits(config, account).await.ok();
            let snapshot = json!({
                "planType": usage.plan_type,
                "primary": usage.primary,
                "secondary": usage.secondary,
            });
            let snapshot = zc_provider_codex::usage_limits::CodexRateLimitSnapshot::from_value(&snapshot).unwrap_or_default();
            let mut limits = zc_provider_codex::usage_limits::codex_rate_limits_to_limits(&snapshot, None, None, &checked_at);
            if let Some(available) = available {
                let mut credits = Map::new();
                credits.insert("availableCount".into(), json!(available.len()));
                if let Some((id, expires_at)) = available.first() {
                    credits.insert("nextCreditId".into(), json!(id));
                    credits.insert("nextExpiresAt".into(), json!(date_parse(expires_at).and_then(iso_from_millis)));
                }
                limits["resetCredits"] = Value::Object(credits);
            }
            let mut account_value = base.clone();
            let plan = usage.plan_type.as_deref().or(account.chatgpt_plan_type.as_deref());
            if let Some(plan) = zc_provider_codex::provider_status::codex_plan_label(plan) {
                account_value.insert("plan".into(), json!(plan));
            }
            account_value.insert("usageLimits".into(), limits);
            Ok(Value::Object(account_value))
        };
        match read.await {
            Ok(account) => account,
            Err(_) => {
                let mut account_value = base;
                account_value.insert(
                    "usageLimits".into(),
                    zc_provider_codex::usage_limits::make_unavailable_usage_limits(
                        &checked_at,
                        "probeFailed",
                        Some("The hub could not read this account's usage."),
                    ),
                );
                Value::Object(account_value)
            }
        }
    }

    /// `readAccounts`: every enabled Codex and Claude account, read four at a time.
    pub async fn read_accounts(&self, config: &SourceConfig) -> Result<Vec<Value>, SourceError> {
        let accounts = self
            .auth_files(config)
            .await
            .map_err(|_| SourceError::new("The hub could not list accounts."))?;
        let accounts: Vec<AuthFile> = accounts
            .into_iter()
            .filter(|account| account.disabled != Some(true) && (account.provider == "codex" || account.provider == "claude"))
            .collect();
        let reads: Vec<futures::future::BoxFuture<'_, Value>> = accounts.iter().map(|account| self.read_account(config, account).boxed()).collect();
        Ok(stream::iter(reads).buffered(4).collect().await)
    }

    /// `consume`: redeems `credit_id` on a Codex account, then clears that account's hub
    /// cooldown. Returns the encoded `ProviderConsumeResetCreditResult`.
    pub async fn consume(&self, config: &SourceConfig, account_id: &str, credit_id: &str) -> Result<Value, SourceError> {
        let operation = async {
            let account = self.auth_files(config).await?.into_iter().find(|account| account.id == account_id);
            let Some(account) = account.filter(|account| account.disabled != Some(true) && account.provider == "codex") else {
                return Err(Failure::Source(SourceError::new("The Codex hub account is missing or disabled.")));
            };
            let redeem_id = credit_redeem_request_id(account.chatgpt_account_id.as_deref().unwrap_or(&account.id), credit_id);
            let body = self
                .api_call(
                    config,
                    &account,
                    &format!("{}/consume", credit_url()),
                    Some(json!({"redeem_request_id": redeem_id, "credit_id": credit_id})),
                )
                .await?;
            let response = json::parse(body.as_bytes()).ok_or(Failure::Schema)?;
            let outcome = match response.get("code").and_then(J::as_str) {
                Some("reset") => "reset",
                Some("nothing_to_reset") => "nothingToReset",
                Some("no_credit") => "noCredit",
                Some("already_redeemed") => "alreadyRedeemed",
                _ => return Err(Failure::Schema),
            };
            if outcome != "reset" && outcome != "alreadyRedeemed" {
                return Ok(json!({"outcome": outcome}));
            }
            let cleared = self.management(config, "reset-quota", Some(json!({"auth_index": account.auth_index}))).await;
            let mut result = Map::new();
            result.insert("outcome".into(), json!(outcome));
            if cleared.is_err() {
                result.insert(
                    "warning".into(),
                    json!("Credit redeemed, but the hub cooldown could not be cleared. Routing may resume after its cooldown expires."),
                );
            }
            Ok(Value::Object(result))
        };
        operation.await.map_err(|failure| match failure {
            Failure::Source(error) => error,
            Failure::Schema => SourceError::new("The hub returned an unexpected reset-credit response."),
        })
    }
}

/// The decoded Claude usage (`ClaudeUsage`), keeping only declared keys.
struct ClaudeUsage {
    five_hour: Value,
    seven_day: Value,
    model_scoped: Vec<Value>,
}

/// `Schema.NullOr(Schema.String)`.
fn null_or_string(value: &J) -> Option<Value> {
    match value {
        J::Null => Some(Value::Null),
        J::Str(text) => Some(json!(text)),
        _ => None,
    }
}

fn decode_claude_window(value: Option<&J>) -> Result<Value, Failure> {
    match value {
        None | Some(J::Null) => Ok(Value::Null),
        Some(window @ J::Obj(_)) => {
            let utilization = window.get("utilization").and_then(J::as_num).ok_or(Failure::Schema)?;
            let resets_at = window.get("resets_at").and_then(null_or_string).ok_or(Failure::Schema)?;
            Ok(json!({"utilization": json::num(utilization), "resets_at": resets_at}))
        }
        Some(_) => Err(Failure::Schema),
    }
}

fn decode_claude_usage(body: &str) -> Result<ClaudeUsage, Failure> {
    let usage = json::parse(body.as_bytes()).filter(|usage| matches!(usage, J::Obj(_))).ok_or(Failure::Schema)?;
    let five_hour = decode_claude_window(usage.get("five_hour"))?;
    let seven_day = decode_claude_window(usage.get("seven_day"))?;
    let mut model_scoped = Vec::new();
    match usage.get("limits") {
        None => {}
        Some(J::Arr(limits)) => {
            for limit in limits {
                if !matches!(limit, J::Obj(_)) {
                    return Err(Failure::Schema);
                }
                let kind = limit.get("kind").and_then(J::as_str).ok_or(Failure::Schema)?;
                let percent = match limit.get("percent") {
                    None | Some(J::Null) => None,
                    Some(J::Num(percent)) => Some(*percent),
                    Some(_) => return Err(Failure::Schema),
                };
                let resets_at = match limit.get("resets_at") {
                    None => Value::Null,
                    Some(value) => null_or_string(value).ok_or(Failure::Schema)?,
                };
                let display_name = match limit.get("scope") {
                    None | Some(J::Null) => None,
                    Some(scope @ J::Obj(_)) => match scope.get("model") {
                        None | Some(J::Null) => None,
                        Some(model @ J::Obj(_)) => Some(model.get("display_name").and_then(J::as_str).ok_or(Failure::Schema)?.to_owned()),
                        Some(_) => return Err(Failure::Schema),
                    },
                    Some(_) => return Err(Failure::Schema),
                };
                if kind == "weekly_scoped" {
                    if let (Some(display_name), Some(percent)) = (display_name, percent) {
                        model_scoped.push(json!({"display_name": display_name, "utilization": json::num(percent), "resets_at": resets_at}));
                    }
                }
            }
        }
        Some(_) => return Err(Failure::Schema),
    }
    Ok(ClaudeUsage {
        five_hour,
        seven_day,
        model_scoped,
    })
}

/// The decoded Codex usage (`CodexUsage`): the snapshot windows as `toWindow` maps them.
struct CodexUsage {
    plan_type: Option<String>,
    primary: Value,
    secondary: Value,
}

fn decode_codex_window(value: Option<&J>) -> Result<Value, Failure> {
    match value {
        None | Some(J::Null) => Ok(Value::Null),
        Some(window @ J::Obj(_)) => {
            let used_percent = window.get("used_percent").and_then(J::as_num).ok_or(Failure::Schema)?;
            let resets_at = match window.get("reset_at") {
                None | Some(J::Null) => Value::Null,
                Some(J::Num(reset_at)) => json::num(*reset_at),
                Some(_) => return Err(Failure::Schema),
            };
            let mut mapped = Map::new();
            mapped.insert("usedPercent".into(), json::num(used_percent));
            mapped.insert("resetsAt".into(), resets_at);
            match window.get("limit_window_seconds") {
                None => {}
                Some(J::Num(seconds)) => {
                    mapped.insert("windowDurationMins".into(), json::num(seconds / 60.0));
                }
                Some(_) => return Err(Failure::Schema),
            }
            Ok(Value::Object(mapped))
        }
        Some(_) => Err(Failure::Schema),
    }
}

fn decode_codex_usage(body: &str) -> Result<CodexUsage, Failure> {
    let usage = json::parse(body.as_bytes()).filter(|usage| matches!(usage, J::Obj(_))).ok_or(Failure::Schema)?;
    let plan_type = match usage.get("plan_type") {
        None => None,
        Some(J::Str(plan)) => Some(plan.clone()),
        Some(_) => return Err(Failure::Schema),
    };
    let (primary, secondary) = match usage.get("rate_limit") {
        Some(J::Null) => (Value::Null, Value::Null),
        Some(rate_limit @ J::Obj(_)) => (
            decode_codex_window(rate_limit.get("primary_window"))?,
            decode_codex_window(rate_limit.get("secondary_window"))?,
        ),
        _ => return Err(Failure::Schema),
    };
    Ok(CodexUsage { plan_type, primary, secondary })
}

#[cfg(test)]
mod tests;
