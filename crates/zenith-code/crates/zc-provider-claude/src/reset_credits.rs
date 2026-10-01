//! `provider/Layers/claudeResetCredits.ts`: Claude banked resets (the CLI's `cedar_ember`
//! program). The grants come from the OAuth usage endpoint and a claim goes to the organization,
//! with the credentials the CLI keeps in its config directory. macOS keeps them in the keychain,
//! so there the feature is not offered.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Map, Value};

const API_BASE: &str = "https://api.anthropic.com";
const PROGRAM: &str = "cedar_ember";

fn grant_id_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-z0-9_-]{1,40}$").expect("valid regex"))
}

fn request_id_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9_-]{1,64}$").expect("valid regex"))
}

fn complete_timestamp_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})$").expect("valid regex"))
}

/// `ClaudeResetCreditError.reason`; the `Display` is the user-facing message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ClaudeResetCreditError {
    #[error("Claude returned a malformed reset credit.")]
    MalformedCredit,
    #[error("Claude could not read its login.")]
    LoginUnreadable,
    #[error("Claude could not read its account.")]
    AccountUnreadable,
    #[error("Sign in to Claude again to redeem resets.")]
    SignedOut,
    #[error("Claude is rate limiting resets. Try again soon.")]
    RateLimited,
    #[error("Claude resets are cooling down. Try again later.")]
    CoolingDown,
    #[error("Claude could not confirm the reset. If you are still limited in a moment, try again.")]
    Unconfirmed,
    #[error("Claude could not redeem the reset.")]
    RequestFailed,
}

impl ClaudeResetCreditError {
    /// `isSettledClaudeResetCreditFailure`: every failure except `requestFailed` and
    /// `unconfirmed` is final; those two retry with the same request id.
    pub fn is_settled(self) -> bool {
        !matches!(self, Self::RequestFailed | Self::Unconfirmed)
    }
}

/// `ProviderConsumeResetCreditOutcome` values a claim can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    Reset,
    NothingToReset,
    AlreadyRedeemed,
    NoCredit,
}

impl ClaimOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reset => "reset",
            Self::NothingToReset => "nothingToReset",
            Self::AlreadyRedeemed => "alreadyRedeemed",
            Self::NoCredit => "noCredit",
        }
    }
}

/// `isFutureTimestamp`: complete, calendar-valid and after `now_ms`.
fn is_future_timestamp(value: &str, now_ms: i64) -> bool {
    if !complete_timestamp_regex().is_match(value) {
        return false;
    }
    let (Ok(year), Ok(month), Ok(day)) = (value[0..4].parse::<i32>(), value[5..7].parse::<u32>(), value[8..10].parse::<u32>()) else {
        return false;
    };
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => return false,
    };
    if day == 0 || day > days_in_month {
        return false;
    }
    zc_core::time::parse_iso_millis(value).is_some_and(|ms| ms > now_ms)
}

struct Grant {
    id: String,
    resets_left: i64,
    ends_at: Option<String>,
    paused: bool,
    usable_now: bool,
}

fn decode_grant(raw: &Value) -> Option<Grant> {
    let id = raw.get("id")?.as_str()?;
    if !grant_id_regex().is_match(id) {
        return None;
    }
    let resets_left = raw.get("resets_left")?.as_i64().filter(|n| *n >= 0)?;
    let ends_at = match raw.get("ends_at") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return None,
    };
    let flag = |key: &str| -> Option<bool> {
        match raw.get(key) {
            None => Some(false),
            Some(Value::Bool(b)) => Some(*b),
            Some(_) => None,
        }
    };
    Some(Grant {
        id: id.to_string(),
        resets_left,
        ends_at,
        paused: flag("paused")?,
        usable_now: flag("usable_now")?,
    })
}

/// `claudeResetCreditsToContract(block, nowMs)`: `ServerProviderResetCredits`, or `None` for an
/// ineligible account. Paused or expired grants do not count.
pub fn claude_reset_credits_to_contract(block: Option<&Value>, now_ms: i64) -> Option<Value> {
    let block = block?.as_object()?;
    if !block.get("eligible")?.as_bool()? {
        return None;
    }
    let grants = match block.get("grants") {
        None => Vec::new(),
        Some(Value::Array(grants)) => grants.clone(),
        Some(_) => return None,
    };
    let next_grant_id = match block.get("next_grant_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return None,
    };
    let live: Vec<Grant> = grants
        .iter()
        .filter_map(decode_grant)
        .filter(|grant| !grant.paused && grant.usable_now && grant.ends_at.as_deref().is_none_or(|ends| is_future_timestamp(ends, now_ms)))
        .collect();
    let next = live.iter().find(|grant| Some(&grant.id) == next_grant_id.as_ref());
    let mut result = Map::new();
    result.insert(
        "availableCount".into(),
        Value::from(if next.is_some() { live.iter().map(|g| g.resets_left).sum::<i64>() } else { 0 }),
    );
    if let Some(expires) = next
        .and_then(|g| g.ends_at.as_deref())
        .filter(|e| !e.is_empty())
        .and_then(zc_core::time::normalize_iso)
    {
        result.insert("nextExpiresAt".into(), Value::String(expires));
    }
    if let Some(next) = next {
        result.insert("nextCreditId".into(), Value::String(next.id.clone()));
    }
    Some(Value::Object(result))
}

/// The HTTP the reset-credit calls need (injectable for tests).
#[async_trait]
pub trait ResetCreditsHttp: Send + Sync {
    /// `GET url` → (status, body).
    async fn get(&self, url: &str, headers: &[(String, String)], timeout: Duration) -> Result<(u16, String), String>;
    /// `POST url` with a JSON body → (status, body).
    async fn post_json(&self, url: &str, headers: &[(String, String)], body: &Value, timeout: Duration) -> Result<(u16, String), String>;
}

/// [`ResetCreditsHttp`] over `reqwest`.
#[derive(Debug, Clone, Default)]
pub struct ReqwestResetCreditsHttp {
    client: reqwest::Client,
}

#[async_trait]
impl ResetCreditsHttp for ReqwestResetCreditsHttp {
    async fn get(&self, url: &str, headers: &[(String, String)], timeout: Duration) -> Result<(u16, String), String> {
        let mut request = self.client.get(url).timeout(timeout);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        Ok((status, response.text().await.map_err(|e| e.to_string())?))
    }

    async fn post_json(&self, url: &str, headers: &[(String, String)], body: &Value, timeout: Duration) -> Result<(u16, String), String> {
        let mut request = self.client.post(url).timeout(timeout).json(body);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        Ok((status, response.text().await.map_err(|e| e.to_string())?))
    }
}

/// Read a JSON file; a missing file reads as `{}`.
fn read_json(file: &Path) -> Result<Value, ()> {
    match std::fs::read_to_string(file) {
        Ok(text) => serde_json::from_str(&text).map_err(|_| ()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(_) => Err(()),
    }
}

/// `readAccessToken`: the OAuth token in `<configDir>/.credentials.json` (never on macOS).
fn read_access_token(config_dir: &Path, platform: &str) -> Result<Option<String>, ()> {
    if platform == "darwin" {
        return Ok(None);
    }
    let credentials = read_json(&config_dir.join(".credentials.json"))?;
    match credentials.get("claudeAiOauth") {
        None => Ok(None),
        Some(Value::Object(oauth)) => match oauth.get("accessToken") {
            None => Ok(None),
            Some(Value::String(token)) => Ok(Some(token.trim().to_string()).filter(|t| !t.is_empty())),
            Some(_) => Err(()),
        },
        Some(_) => Err(()),
    }
}

fn claude_headers(token: &str, version: &str) -> Vec<(String, String)> {
    vec![
        ("authorization".into(), format!("Bearer {token}")),
        ("anthropic-beta".into(), "oauth-2025-04-20".into()),
        ("user-agent".into(), format!("claude-cli/{version} (external, cli)")),
    ]
}

/// `readClaudeResetCredits(configDir, version)`: any failure reads as "no resets".
pub async fn read_claude_reset_credits(http: &dyn ResetCreditsHttp, config_dir: &Path, version: &str, platform: &str, now_ms: i64) -> Option<Value> {
    let token = read_access_token(config_dir, platform).ok()??;
    let url = format!("{API_BASE}/api/oauth/usage?cedar_ember=1&skip_spend=1");
    let headers = claude_headers(&token, version);
    let fetch = http.get(&url, &headers, Duration::from_secs(10));
    let (status, body) = tokio::time::timeout(Duration::from_secs(10), fetch).await.ok()?.ok()?;
    if !(200..300).contains(&status) {
        return None;
    }
    let body: Value = serde_json::from_str(&body).ok()?;
    let block = match body.get("cedar_ember") {
        None | Some(Value::Null) => return claude_reset_credits_to_contract(None, now_ms),
        Some(block) => block,
    };
    claude_reset_credits_to_contract(Some(block), now_ms)
}

/// `claudeAccountConfigPath(configDir)`: the CLI's account record.
pub fn claude_account_config_path(config_dir: Option<&Path>) -> PathBuf {
    match config_dir {
        Some(dir) => dir.join(".claude.json"),
        None => zc_core::paths::home_dir().join(".claude.json"),
    }
}

/// `consumeClaudeResetCredit`: claim `grant_id`; `request_id` is the idempotency key.
pub async fn consume_claude_reset_credit(
    http: &dyn ResetCreditsHttp,
    config_dir: &Path,
    account_config_path: &Path,
    version: &str,
    grant_id: &str,
    request_id: &str,
    platform: &str,
) -> Result<ClaimOutcome, ClaudeResetCreditError> {
    if !grant_id_regex().is_match(grant_id) || !request_id_regex().is_match(request_id) {
        return Err(ClaudeResetCreditError::MalformedCredit);
    }
    let token = read_access_token(config_dir, platform).map_err(|()| ClaudeResetCreditError::LoginUnreadable)?;
    let config = read_json(account_config_path).map_err(|()| ClaudeResetCreditError::AccountUnreadable)?;
    let organization = match config.get("oauthAccount") {
        None => None,
        Some(Value::Object(account)) => match account.get("organizationUuid") {
            None => None,
            Some(Value::String(org)) => Some(org.trim().to_string()).filter(|o| !o.is_empty()),
            Some(_) => return Err(ClaudeResetCreditError::AccountUnreadable),
        },
        Some(_) => return Err(ClaudeResetCreditError::AccountUnreadable),
    };
    let (Some(token), Some(organization)) = (token, organization) else {
        return Err(ClaudeResetCreditError::SignedOut);
    };
    let encoded: String = organization
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    let url = format!("{API_BASE}/api/organizations/{encoded}/reset_rate_limits");
    let body = json!({ "program": PROGRAM, "grant_id": grant_id, "request_id": request_id });
    let headers = claude_headers(&token, version);
    let send = http.post_json(&url, &headers, &body, Duration::from_secs(25));
    let (status, text) = tokio::time::timeout(Duration::from_secs(25), send)
        .await
        .map_err(|_| ClaudeResetCreditError::RequestFailed)?
        .map_err(|_| ClaudeResetCreditError::RequestFailed)?;
    match status {
        429 => return Err(ClaudeResetCreditError::RateLimited),
        401 | 403 => return Err(ClaudeResetCreditError::SignedOut),
        200..=299 => {}
        _ => return Err(ClaudeResetCreditError::RequestFailed),
    }
    let parsed: Value = serde_json::from_str(&text).map_err(|_| ClaudeResetCreditError::RequestFailed)?;
    match parsed.get("result").and_then(Value::as_str) {
        Some("reset") => Ok(ClaimOutcome::Reset),
        Some("not_limited") => Ok(ClaimOutcome::NothingToReset),
        Some("already_used") => Ok(ClaimOutcome::AlreadyRedeemed),
        Some("ineligible") => Ok(ClaimOutcome::NoCredit),
        Some("cooldown") => Err(ClaudeResetCreditError::CoolingDown),
        Some("unavailable") => Err(ClaudeResetCreditError::Unconfirmed),
        _ => Err(ClaudeResetCreditError::RequestFailed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn now() -> i64 {
        zc_core::time::parse_iso_millis("2026-09-22T12:00:00.000Z").unwrap()
    }

    fn grant(overrides: Value) -> Value {
        let mut base = json!({"id": "grant_a", "resets_left": 1, "usable_now": true});
        for (k, v) in overrides.as_object().unwrap() {
            base[k] = v.clone();
        }
        base
    }

    #[test]
    fn counts_live_grants_and_pins_the_next_usable_one() {
        let block = json!({
            "eligible": true, "next_grant_id": "grant_a",
            "grants": [
                grant(json!({"resets_left": 2, "ends_at": "2026-10-01T00:00:00Z"})),
                grant(json!({"id": "paused", "paused": true})),
                grant(json!({"id": "expired", "ends_at": "2026-09-01T00:00:00Z"})),
                grant(json!({"id": "garbled", "ends_at": "not a date"})),
                grant(json!({"id": "date_only", "ends_at": "2026-10-01"})),
                grant(json!({"id": "impossible", "ends_at": "2027-02-30T00:00:00Z"})),
                grant(json!({"id": "empty", "ends_at": ""})),
                grant(json!({"id": "Not Valid"})),
                grant(json!({"id": "grant_b", "resets_left": 3, "usable_now": false}))
            ]
        });
        assert_eq!(
            claude_reset_credits_to_contract(Some(&block), now()),
            Some(json!({"availableCount": 2, "nextCreditId": "grant_a", "nextExpiresAt": "2026-10-01T00:00:00.000Z"}))
        );
    }

    #[test]
    fn offers_nothing_without_a_usable_next_grant_or_an_eligible_account() {
        let unusable = json!({"eligible": true, "next_grant_id": "grant_a", "grants": [grant(json!({"usable_now": false}))]});
        assert_eq!(claude_reset_credits_to_contract(Some(&unusable), now()), Some(json!({"availableCount": 0})));
        let no_next = json!({"eligible": true, "grants": [grant(json!({}))]});
        assert_eq!(claude_reset_credits_to_contract(Some(&no_next), now()), Some(json!({"availableCount": 0})));
        let ineligible = json!({"eligible": false, "grants": [grant(json!({}))]});
        assert_eq!(claude_reset_credits_to_contract(Some(&ineligible), now()), None);
        assert_eq!(claude_reset_credits_to_contract(None, now()), None);
    }

    /// `(url, headers, body)`.
    type Request = (String, Vec<(String, String)>, Option<Value>);

    struct FakeHttp {
        status: u16,
        body: Value,
        requests: Mutex<Vec<Request>>,
    }

    #[async_trait]
    impl ResetCreditsHttp for FakeHttp {
        async fn get(&self, url: &str, headers: &[(String, String)], _: Duration) -> Result<(u16, String), String> {
            self.requests.lock().unwrap().push((url.to_string(), headers.to_vec(), None));
            Ok((self.status, self.body.to_string()))
        }
        async fn post_json(&self, url: &str, headers: &[(String, String)], body: &Value, _: Duration) -> Result<(u16, String), String> {
            self.requests.lock().unwrap().push((url.to_string(), headers.to_vec(), Some(body.clone())));
            Ok((self.status, self.body.to_string()))
        }
    }

    fn write_login() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".credentials.json"), r#"{"claudeAiOauth":{"accessToken":"oauth-token"}}"#).unwrap();
        let account = dir.path().join(".claude.json");
        std::fs::write(&account, r#"{"oauthAccount":{"organizationUuid":"org-1"}}"#).unwrap();
        (dir, account)
    }

    #[tokio::test]
    async fn reads_the_grants_with_the_clis_request() {
        let (dir, _) = write_login();
        let http = FakeHttp {
            status: 200,
            body: json!({"cedar_ember": {"eligible": true, "next_grant_id": "grant_a", "grants": [grant(json!({}))]}}),
            requests: Mutex::default(),
        };
        let credits = read_claude_reset_credits(&http, dir.path(), "2.1.0", "linux", now()).await;
        assert_eq!(credits, Some(json!({"availableCount": 1, "nextCreditId": "grant_a"})));
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests[0].0, "https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1");
        assert!(requests[0].1.contains(&("authorization".to_string(), "Bearer oauth-token".to_string())));
        assert!(requests[0].1.contains(&("anthropic-beta".to_string(), "oauth-2025-04-20".to_string())));
        assert!(requests[0]
            .1
            .contains(&("user-agent".to_string(), "claude-cli/2.1.0 (external, cli)".to_string())));
        assert_eq!(requests[0].2, None, "a GET carries no body");
    }

    #[tokio::test]
    async fn reads_nothing_from_keychain_logins_or_failed_requests() {
        let (dir, _) = write_login();
        let refuse = FakeHttp {
            status: 500,
            body: json!({}),
            requests: Mutex::default(),
        };
        assert_eq!(read_claude_reset_credits(&refuse, dir.path(), "2.1.0", "darwin", now()).await, None);
        assert!(refuse.requests.lock().unwrap().is_empty());
        let limited = FakeHttp {
            status: 429,
            body: json!({}),
            requests: Mutex::default(),
        };
        assert_eq!(read_claude_reset_credits(&limited, dir.path(), "2.1.0", "linux", now()).await, None);
    }

    #[tokio::test]
    async fn claims_a_grant_and_maps_every_answer() {
        let (dir, account) = write_login();
        for (status, body, expected) in [
            (200, json!({"result": "reset"}), Ok(ClaimOutcome::Reset)),
            (200, json!({"result": "not_limited"}), Ok(ClaimOutcome::NothingToReset)),
            (200, json!({"result": "already_used"}), Ok(ClaimOutcome::AlreadyRedeemed)),
            (200, json!({"result": "ineligible"}), Ok(ClaimOutcome::NoCredit)),
            (200, json!({"result": "cooldown"}), Err(ClaudeResetCreditError::CoolingDown)),
            (200, json!({"result": "unavailable"}), Err(ClaudeResetCreditError::Unconfirmed)),
            (429, json!({}), Err(ClaudeResetCreditError::RateLimited)),
            (401, json!({}), Err(ClaudeResetCreditError::SignedOut)),
            (500, json!({}), Err(ClaudeResetCreditError::RequestFailed)),
        ] {
            let http = FakeHttp {
                status,
                body,
                requests: Mutex::default(),
            };
            assert_eq!(
                consume_claude_reset_credit(&http, dir.path(), &account, "2.1.0", "grant_a", "r-1", "linux").await,
                expected
            );
        }
        let http = FakeHttp {
            status: 200,
            body: json!({"result": "reset"}),
            requests: Mutex::default(),
        };
        consume_claude_reset_credit(&http, dir.path(), &account, "2.1.0", "grant_a", "r-1", "linux")
            .await
            .unwrap();
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests[0].0, "https://api.anthropic.com/api/organizations/org-1/reset_rate_limits");
        assert!(requests[0].1.contains(&("authorization".to_string(), "Bearer oauth-token".to_string())));
        assert_eq!(
            requests[0].2,
            Some(json!({"program": "cedar_ember", "grant_id": "grant_a", "request_id": "r-1"}))
        );
        assert_eq!(
            futures::executor::block_on(consume_claude_reset_credit(&http, dir.path(), &account, "2.1.0", "Bad Id", "r-1", "linux")),
            Err(ClaudeResetCreditError::MalformedCredit)
        );
        assert!(!ClaudeResetCreditError::Unconfirmed.is_settled());
        assert!(ClaudeResetCreditError::CoolingDown.is_settled());
    }

    #[tokio::test]
    async fn settles_answered_failures_and_retries_unanswered_ones() {
        let (dir, account) = write_login();
        // Claude answered, so a retry must be a new claim.
        for (status, body) in [(200, json!({"result": "cooldown"})), (429, json!({})), (401, json!({}))] {
            let http = FakeHttp {
                status,
                body,
                requests: Mutex::default(),
            };
            let failure = consume_claude_reset_credit(&http, dir.path(), &account, "2.1.0", "grant_a", "r-1", "linux")
                .await
                .unwrap_err();
            assert!(failure.is_settled(), "{failure:?} should be settled");
        }
        // No answer, or Claude could not confirm the claim: a retry is the same claim.
        for (status, body) in [(500, json!({})), (200, json!({"result": "unavailable"}))] {
            let http = FakeHttp {
                status,
                body,
                requests: Mutex::default(),
            };
            let failure = consume_claude_reset_credit(&http, dir.path(), &account, "2.1.0", "grant_a", "r-1", "linux")
                .await
                .unwrap_err();
            assert!(!failure.is_settled(), "{failure:?} should not be settled");
        }
    }

    /// Answers a claim with headers but never finishes the body.
    struct StallingHttp;

    #[async_trait]
    impl ResetCreditsHttp for StallingHttp {
        async fn get(&self, _: &str, _: &[(String, String)], _: Duration) -> Result<(u16, String), String> {
            std::future::pending().await
        }
        async fn post_json(&self, _: &str, _: &[(String, String)], _: &Value, _: Duration) -> Result<(u16, String), String> {
            std::future::pending().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn times_out_a_stalled_claim_body() {
        let (dir, account) = write_login();
        let claim = tokio::spawn(async move { consume_claude_reset_credit(&StallingHttp, dir.path(), &account, "2.1.0", "grant_a", "r-1", "linux").await });
        tokio::time::advance(Duration::from_secs(26)).await;
        assert_eq!(claim.await.unwrap(), Err(ClaudeResetCreditError::RequestFailed));
    }

    #[tokio::test]
    async fn refuses_malformed_ids_without_sending_anything() {
        let (dir, account) = write_login();
        for (grant_id, request_id) in [("Bad Grant", "r-1"), ("grant_a", "has space")] {
            let http = FakeHttp {
                status: 200,
                body: json!({"result": "reset"}),
                requests: Mutex::default(),
            };
            assert_eq!(
                consume_claude_reset_credit(&http, dir.path(), &account, "2.1.0", grant_id, request_id, "linux").await,
                Err(ClaudeResetCreditError::MalformedCredit)
            );
            assert!(http.requests.lock().unwrap().is_empty());
        }
    }
}
