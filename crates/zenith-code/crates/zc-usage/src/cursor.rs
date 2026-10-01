//! Cursor account usage (`cursorUsageReader.ts`): the dashboard API, which includes headless
//! agents and reports fresh input apart from cache reads, read with the Cursor CLI's login
//! (`auth.json`, or the macOS Keychain entry `cursor-access-token` / `cursor-user`).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use futures::future::{BoxFuture, FutureExt, Shared};
use regex::Regex;
use sha2::{Digest, Sha256};
use zc_contracts::UsageProviderKind;

use crate::collate::locale_compare;
use crate::json::{self, J};
use crate::records::{Totals, UsageRecord};

pub const CURSOR_USAGE_URL: &str = "https://cursor.com/api/dashboard/get-filtered-usage-events";
const PAGE_SIZE: usize = 1000;
const ACCOUNT_DEADLINE: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A dashboard response: status and body.
#[derive(Debug, Clone)]
pub struct HttpReply {
    pub status: u16,
    pub body: Vec<u8>,
}

/// The dashboard transport (`fetch` in TS; a fake in tests).
#[async_trait]
pub trait CursorHttp: Send + Sync {
    /// POST `body` to `url` with `headers`, without following redirects. `Err` is a network
    /// failure or timeout.
    async fn post(&self, url: &str, headers: &[(&str, String)], body: String, timeout: Duration) -> Result<HttpReply, String>;
}

/// [`CursorHttp`] over reqwest.
#[derive(Clone)]
pub struct ReqwestCursorHttp {
    client: reqwest::Client,
}

impl Default for ReqwestCursorHttp {
    fn default() -> Self {
        Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_default(),
        }
    }
}

#[async_trait]
impl CursorHttp for ReqwestCursorHttp {
    async fn post(&self, url: &str, headers: &[(&str, String)], body: String, timeout: Duration) -> Result<HttpReply, String> {
        let mut request = self.client.post(url).timeout(timeout).body(body);
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        let response = request.send().await.map_err(|error| error.to_string())?;
        // `redirect: "error"`.
        if response.status().is_redirection() {
            return Err("redirected".to_owned());
        }
        let status = response.status().as_u16();
        let body = response.bytes().await.map_err(|error| error.to_string())?;
        Ok(HttpReply { status, body: body.to_vec() })
    }
}

/// Why the Keychain token could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeychainError {
    /// Nobody answered the macOS access prompt in time (`CursorKeychainTimeoutError`).
    Timeout,
    Failed,
}

/// Reads the Cursor CLI's Keychain token (`readMacCursorAccessToken`).
#[async_trait]
pub trait KeychainToken: Send + Sync {
    async fn read(&self) -> Result<Option<String>, KeychainError>;
}

/// Where the account token comes from.
pub enum CredentialSource<'a> {
    File(&'a Path),
    Keychain(&'a dyn KeychainToken),
}

/// `CursorAccountUsageReadResult`.
#[derive(Debug, Clone, PartialEq)]
pub struct CursorAccountUsage {
    pub account_key: Option<String>,
    pub records: Vec<UsageRecord>,
    pub missing: bool,
    pub error: Option<String>,
}

static RATE_MODEL_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:-thinking)?(?:-(?:none|minimal|low|medium|high|xhigh|max))?(?:-fast)?$").expect("valid regex"));

/// `cursorRateModel`: Cursor's tiered names (`cursor-grok-4.6-high-fast`,
/// `claude-fable-5-1-thinking-high`) to the base model's rate-table key.
pub fn cursor_rate_model(model: &str) -> String {
    let without_prefix = model.strip_prefix("cursor-").unwrap_or(model);
    let base = RATE_MODEL_SUFFIX.replace(without_prefix, "");
    if base.starts_with("grok-") {
        format!("xai/{base}")
    } else {
        base.into_owned()
    }
}

fn sha256_hex(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `canonicalJson`: object keys sorted with `localeCompare`.
fn canonical_json(value: &J) -> String {
    match value {
        J::Arr(items) => format!("[{}]", items.iter().map(canonical_json).collect::<Vec<_>>().join(",")),
        J::Obj(object) => {
            let mut entries = object.entries();
            entries.sort_by(|(a, _), (b, _)| locale_compare(a, b));
            format!(
                "{{{}}}",
                entries
                    .iter()
                    .map(|(key, entry)| format!("{}:{}", json::quote(key), canonical_json(entry)))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        other => json::stringify(other),
    }
}

/// The longest exact suffix/prefix overlap, in linear time (KMP failure function).
fn boundary_overlap(previous: &[String], current: &[String]) -> usize {
    let empty = String::new();
    let sequence: Vec<&String> = current.iter().chain(std::iter::once(&empty)).chain(previous.iter()).collect();
    let mut lengths = vec![0usize; sequence.len()];
    for index in 1..sequence.len() {
        let mut length = lengths[index - 1];
        while length > 0 && sequence[index] != sequence[length] {
            length = lengths[length - 1];
        }
        if sequence[index] == sequence[length] {
            length += 1;
        }
        lengths[index] = length;
    }
    lengths.last().copied().unwrap_or(0)
}

/// Node's lenient `Buffer.from(text, "base64url")`.
fn decode_base64url(text: &str) -> Vec<u8> {
    let mut cleaned: String = text
        .chars()
        .take_while(|ch| *ch != '=')
        .filter_map(|ch| match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' => Some(ch),
            '+' => Some('-'),
            '/' => Some('_'),
            _ => None,
        })
        .collect();
    if cleaned.len() % 4 == 1 {
        cleaned.pop();
    }
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::URL_SAFE,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    );
    engine.decode(cleaned).unwrap_or_default()
}

fn failure(account_key: Option<String>) -> CursorAccountUsage {
    CursorAccountUsage {
        account_key,
        records: Vec::new(),
        missing: false,
        error: Some("Cursor account usage could not be read.".to_owned()),
    }
}

enum ReadError {
    Failed,
    Denied,
}

/// `readCursorAccountUsage`.
pub async fn read_cursor_account_usage(credential: CredentialSource<'_>, since_ms: f64, end_ms: f64, http: &dyn CursorHttp) -> CursorAccountUsage {
    let token = match &credential {
        CredentialSource::File(path) => match tokio::fs::read(path).await {
            Ok(bytes) => match json::parse(&bytes) {
                Some(document) => Ok(document
                    .as_obj()
                    .and_then(|object| object.get("accessToken"))
                    .and_then(J::as_str)
                    .map(str::to_owned)),
                None => Err(Some("Cursor credentials could not be read.")),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(None),
            Err(_) => Err(Some("Cursor credentials could not be read.")),
        },
        CredentialSource::Keychain(keychain) => keychain.read().await.map_err(|error| {
            Some(match error {
                KeychainError::Timeout => "Allow Keychain access on the Mac running zenith, then refresh.",
                KeychainError::Failed => "Cursor Keychain credentials could not be read.",
            })
        }),
    };
    let token = match token {
        Ok(token) => token,
        Err(error) => {
            return CursorAccountUsage {
                account_key: None,
                records: Vec::new(),
                missing: error.is_none(),
                error: error.map(str::to_owned),
            };
        }
    };
    let Some(access_token) = token.filter(|token| !token.is_empty()) else {
        return CursorAccountUsage {
            account_key: None,
            records: Vec::new(),
            missing: true,
            error: match credential {
                CredentialSource::File(_) => None,
                CredentialSource::Keychain(_) => Some("Cursor account history needs a macOS Keychain CLI login on this server.".to_owned()),
            },
        };
    };

    // The account identity: the token's `sub`, hashed so the key never carries it.
    let payload = access_token.split('.').nth(1).unwrap_or("");
    let subject = json::parse(&decode_base64url(payload))
        .and_then(|claims| claims.as_obj().and_then(|object| object.get("sub")).and_then(J::as_str).map(str::to_owned))
        .filter(|subject| !subject.is_empty());
    let Some(subject) = subject else {
        return failure(None);
    };
    let user_id = subject.rsplit('|').next().unwrap_or("").to_owned();
    if user_id.is_empty() {
        return failure(None);
    }
    let account_key = sha256_hex(&subject);
    match read_pages(&access_token, &user_id, &account_key, since_ms, end_ms, http).await {
        Ok(records) => CursorAccountUsage {
            account_key: Some(account_key),
            records,
            missing: false,
            error: None,
        },
        Err(ReadError::Denied) => CursorAccountUsage {
            account_key: Some(account_key),
            records: Vec::new(),
            missing: false,
            error: Some("Sign in to Cursor again to read account usage.".to_owned()),
        },
        Err(ReadError::Failed) => failure(Some(account_key)),
    }
}

/// `encodeURIComponent`.
fn encode_uri_component(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

async fn read_pages(
    access_token: &str,
    user_id: &str,
    account_key: &str,
    since_ms: f64,
    end_ms: f64,
    http: &dyn CursorHttp,
) -> Result<Vec<UsageRecord>, ReadError> {
    if !since_ms.is_finite() || !end_ms.is_finite() || since_ms < 0.0 || since_ms > end_ms {
        return Err(ReadError::Failed);
    }
    let deadline = Instant::now() + ACCOUNT_DEADLINE;
    let cookie = format!("WorkosCursorSessionToken={}", encode_uri_component(&format!("{user_id}::{access_token}")));
    let mut pages: Vec<Vec<J>> = Vec::new();
    let mut total: Option<f64> = None;
    let mut page = 1usize;
    loop {
        // A count can include overlapping page boundaries: allow room to reconcile them.
        #[allow(clippy::cast_precision_loss)]
        let limit = match total {
            None => 1000.0,
            Some(total) => (total / PAGE_SIZE as f64).ceil() * 2.0 + 1.0,
        };
        #[allow(clippy::cast_precision_loss)]
        if page as f64 > limit {
            return Err(ReadError::Failed);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ReadError::Failed);
        }
        let body = format!(
            "{{\"page\":{page},\"pageSize\":{PAGE_SIZE},\"startDate\":{},\"endDate\":{}}}",
            json::quote(&json::number_to_string(since_ms)),
            json::quote(&json::number_to_string(end_ms))
        );
        let headers = [
            ("Content-Type", "application/json".to_owned()),
            ("Origin", "https://cursor.com".to_owned()),
            ("Cookie", cookie.clone()),
        ];
        let reply = http
            .post(CURSOR_USAGE_URL, &headers, body, REQUEST_TIMEOUT.min(remaining))
            .await
            .map_err(|_| ReadError::Failed)?;
        if reply.status == 401 || reply.status == 403 {
            return Err(ReadError::Denied);
        }
        if !(200..300).contains(&reply.status) {
            return Err(ReadError::Failed);
        }
        let parsed = json::parse(&reply.body).ok_or(ReadError::Failed)?;
        let J::Obj(body) = parsed else {
            return Err(ReadError::Failed);
        };
        if body.contains_key("error") || body.contains_key("message") || body.contains_key("code") {
            return Err(ReadError::Failed);
        }
        let keys: Vec<&str> = body.entries().into_iter().map(|(key, _)| key).collect();
        let count: Option<&J> = if keys.is_empty() {
            Some(&J::Num(0.0))
        } else {
            body.get("totalUsageEventsCount")
        };
        let empty = J::Arr(Vec::new());
        let events: Option<&J> = if keys.is_empty() || (keys.len() == 1 && keys[0] == "totalUsageEventsCount") {
            Some(&empty)
        } else {
            body.get("usageEventsDisplay")
        };
        let count = match count {
            None => None,
            Some(J::Num(count)) if count.fract() == 0.0 && (0.0..=9_007_199_254_740_991.0).contains(count) && total.is_none_or(|total| total == *count) => {
                Some(*count)
            }
            Some(_) => return Err(ReadError::Failed),
        };
        let Some(J::Arr(events)) = events else {
            return Err(ReadError::Failed);
        };
        if events.len() > PAGE_SIZE || (count.is_none() && !matches!(body.get("usageEventsDisplay"), Some(J::Arr(_)))) {
            return Err(ReadError::Failed);
        }
        if count.is_some() {
            total = count;
        }
        let full = events.len() == PAGE_SIZE;
        pages.push(events.clone());
        if !full {
            break;
        }
        page += 1;
    }

    let raw_count: usize = pages.iter().map(Vec::len).sum();
    #[allow(clippy::cast_precision_loss)]
    if total.is_some_and(|total| (raw_count as f64) < total) {
        return Err(ReadError::Failed);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
    let mut removals_remaining = total.map_or(0usize, |total| (raw_count as f64 - total) as usize);
    let mut previous_keys: Vec<String> = Vec::new();
    let mut occurrences: HashMap<String, usize> = HashMap::new();
    let mut records = Vec::new();
    for events in &pages {
        let event_keys: Vec<String> = if removals_remaining > 0 {
            events.iter().map(|event| sha256_hex(&canonical_json(event))).collect()
        } else {
            Vec::new()
        };
        let removal_count = removals_remaining.min(boundary_overlap(&previous_keys, &event_keys));
        removals_remaining -= removal_count;
        previous_keys = event_keys;
        for raw in &events[removal_count..] {
            let event = if matches!(raw, J::Obj(_)) { Some(raw) } else { None };
            let token_usage = json::get(event, "tokenUsage");
            if matches!(token_usage, None | Some(J::Null)) {
                continue;
            }
            let usage = token_usage.filter(|usage| matches!(usage, J::Obj(_)));
            for key in ["inputTokens", "outputTokens", "cacheReadTokens", "cacheWriteTokens", "totalCents"] {
                if let Some(value) = json::get(usage, key) {
                    match value.as_num() {
                        Some(number) if number.is_finite() && number >= 0.0 => {}
                        _ => return Err(ReadError::Failed),
                    }
                }
            }
            let timestamp = json::get(event, "timestamp");
            let timestamp_ms = match timestamp {
                Some(J::Str(text)) if !json::js_trim(text).is_empty() => Some(json::string_to_number(text)),
                Some(J::Num(number)) => Some(*number),
                _ => None,
            };
            let model = json::get(event, "model").and_then(J::as_str).filter(|model| !model.is_empty());
            let (Some(timestamp_ms), Some(model)) = (timestamp_ms.filter(|value| value.is_finite()), model) else {
                return Err(ReadError::Failed);
            };
            if timestamp_ms < since_ms || timestamp_ms > end_ms {
                continue;
            }
            let tokens = |key: &str| crate::records::int(json::get(usage, key));
            let totals = Totals {
                uncached_input_tokens: tokens("inputTokens"),
                cached_input_tokens: tokens("cacheReadTokens"),
                cache_creation_tokens: tokens("cacheWriteTokens"),
                output_tokens: tokens("outputTokens"),
                reasoning_tokens: 0.0,
            };
            let reported_cost_usd = json::get(usage, "totalCents").and_then(J::as_num).map(|cents| cents / 100.0);
            let session_id = json::get(event, "conversationId").and_then(J::as_str).unwrap_or("");
            // No event id: identical billed rows stay distinct through an occurrence index.
            let key = sha256_hex(&format!(
                "[{},{},{},{},{}]",
                zc_providers::js_json::format_js_number(timestamp_ms),
                json::quote(model),
                json::quote(session_id),
                totals.stringify(),
                reported_cost_usd.map_or_else(|| "null".to_owned(), zc_providers::js_json::format_js_number)
            ));
            let occurrence = occurrences.entry(key.clone()).or_insert(0);
            let dedupe_key = format!("cursor-account:{account_key}:{key}:{occurrence}");
            *occurrence += 1;
            records.push(UsageRecord {
                provider: UsageProviderKind::Cursor,
                timestamp_ms,
                model: Arc::from(model),
                rate_model: Some(cursor_rate_model(model)),
                session_id: Arc::from(session_id),
                totals,
                reported_cost_usd,
                fast: false,
                dedupe_key: Some(dedupe_key),
            });
        }
    }
    if removals_remaining != 0 {
        return Err(ReadError::Failed);
    }
    Ok(records)
}

/* ---------------------------------------------------------------------------------------- */
/* Keychain                                                                                 */
/* ---------------------------------------------------------------------------------------- */

const KEYCHAIN_CACHE: Duration = Duration::from_secs(5 * 60);
const KEYCHAIN_PROMPT_TIMEOUT: Duration = Duration::from_secs(30);

type PendingRead = Shared<BoxFuture<'static, Result<Option<String>, KeychainError>>>;

struct CachedState {
    cached: Option<(String, Instant)>,
    pending: Option<PendingRead>,
}

/// `makeCachedCursorAccessTokenReader`: one Keychain request shared across callers, the token
/// cached for five minutes, and callers giving up after 30 s while the read stays in flight
/// (macOS shows the prompt on the server's own screen, which a remote client cannot answer).
pub struct CachedKeychainToken {
    read: Arc<dyn Fn() -> BoxFuture<'static, Result<Option<String>, KeychainError>> + Send + Sync>,
    state: Arc<std::sync::Mutex<CachedState>>,
    timeout: Duration,
}

impl CachedKeychainToken {
    pub fn new(read: Arc<dyn Fn() -> BoxFuture<'static, Result<Option<String>, KeychainError>> + Send + Sync>, timeout: Duration) -> Self {
        Self {
            read,
            state: Arc::new(std::sync::Mutex::new(CachedState { cached: None, pending: None })),
            timeout,
        }
    }
}

#[async_trait]
impl KeychainToken for CachedKeychainToken {
    async fn read(&self) -> Result<Option<String>, KeychainError> {
        let pending = {
            let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((token, until)) = &state.cached {
                if *until > Instant::now() {
                    return Ok(Some(token.clone()));
                }
            }
            if let Some(pending) = &state.pending {
                pending.clone()
            } else {
                let read = (self.read)();
                let shared_state = self.state.clone();
                let future = async move {
                    let result = read.await;
                    let mut state = shared_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    state.cached = match &result {
                        Ok(Some(token)) if !token.is_empty() => Some((token.clone(), Instant::now() + KEYCHAIN_CACHE)),
                        _ => None,
                    };
                    state.pending = None;
                    result
                }
                .boxed()
                .shared();
                // Keep reading even when every caller gives up.
                tokio::spawn(future.clone());
                state.pending = Some(future.clone());
                future
            }
        };
        match tokio::time::timeout(self.timeout, pending).await {
            Ok(result) => result,
            Err(_) => Err(KeychainError::Timeout),
        }
    }
}

/// The process-wide reader of the Cursor CLI's default macOS credential, shared by usage
/// history and (later) Cursor usage limits.
pub fn shared_keychain_token() -> Arc<CachedKeychainToken> {
    static READER: OnceLock<Arc<CachedKeychainToken>> = OnceLock::new();
    READER
        .get_or_init(|| {
            Arc::new(CachedKeychainToken::new(
                Arc::new(|| async { tokio::task::spawn_blocking(read_keychain_entry).await.unwrap_or(Err(KeychainError::Failed)) }.boxed()),
                KEYCHAIN_PROMPT_TIMEOUT,
            ))
        })
        .clone()
}

#[cfg(target_os = "macos")]
fn read_keychain_entry() -> Result<Option<String>, KeychainError> {
    let entry = keyring::Entry::new("cursor-access-token", "cursor-user").map_err(|_| KeychainError::Failed)?;
    match entry.get_password() {
        Ok(password) => Ok(Some(password)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(_) => Err(KeychainError::Failed),
    }
}

#[cfg(not(target_os = "macos"))]
fn read_keychain_entry() -> Result<Option<String>, KeychainError> {
    Err(KeychainError::Failed)
}

#[cfg(test)]
mod tests;
