//! `SourceControlRateLimit.ts`: per provider, host and credential pauses after a rate-limit
//! answer, kept in memory.
//!
//! - A pause lasts until the provider's reset time when one is known, otherwise 30 s doubling per
//!   consecutive rate limit up to 15 min.
//! - `check` hands out a lease (the entry's generation); `recordRateLimit` / `recordSuccess`
//!   take it back, so an answer from an older request can neither clear a newer pause nor
//!   restart the backoff.
//! - Interactive requests may pass a pause (`allow_paused`) without clearing it.
//!
//! The Effect `CredentialScope` reference is the task-local [`CREDENTIAL_SCOPE`]: wrap a future in
//! [`with_credential_scope`] to account its requests to another credential.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use zc_contracts::SourceControlProviderKind;

use crate::errors::{error_defect, CauseError};
use crate::util::{date_parse_millis, js_trim, SharedClock};

const FALLBACK_COOLDOWN_MS: i64 = 30_000;
const MAX_FALLBACK_COOLDOWN_MS: i64 = 15 * 60_000;
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

tokio::task_local! {
    /// `CredentialScope`: who the quota of the current request belongs to (`""` = ambient).
    pub static CREDENTIAL_SCOPE: String;
}

/// The current `CredentialScope` (`""` outside any scope).
pub fn current_credential_scope() -> String {
    CREDENTIAL_SCOPE.try_with(Clone::clone).unwrap_or_default()
}

/// Runs `future` with `CredentialScope` set to `scope`.
pub async fn with_credential_scope<F: std::future::Future>(scope: impl Into<String>, future: F) -> F::Output {
    CREDENTIAL_SCOPE.scope(scope.into(), future).await
}

/// `{provider, host}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitKey {
    pub provider: SourceControlProviderKind,
    pub host: String,
}

impl RateLimitKey {
    pub fn new(provider: SourceControlProviderKind, host: impl Into<String>) -> Self {
        Self { provider, host: host.into() }
    }
}

/// `SourceControlRateLimitPausedError`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "_tag", rename = "SourceControlRateLimitPausedError", rename_all = "camelCase")]
pub struct SourceControlRateLimitPausedError {
    pub provider: SourceControlProviderKind,
    pub host: String,
    pub retry_at: i64,
}

impl SourceControlRateLimitPausedError {
    pub fn detail(&self) -> String {
        format!("{} requests to {} are paused until the rate limit resets.", self.provider.as_str(), self.host)
    }

    pub fn message(&self) -> String {
        self.detail()
    }
}

impl std::fmt::Display for SourceControlRateLimitPausedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for SourceControlRateLimitPausedError {}

impl CauseError for SourceControlRateLimitPausedError {
    fn defect(&self) -> serde_json::Value {
        error_defect("SourceControlRateLimitPausedError", self.message(), None)
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    attempt: u32,
    generation: u64,
    retry_at: i64,
}

fn normalized_key(key: &RateLimitKey, scope: &str) -> String {
    format!("{}\0{}\0{scope}", key.provider.as_str(), js_trim(&key.host).to_lowercase())
}

fn fallback_cooldown_ms(attempt: u32) -> i64 {
    let exponent = attempt.saturating_sub(1).min(40);
    FALLBACK_COOLDOWN_MS.saturating_mul(1_i64 << exponent).min(MAX_FALLBACK_COOLDOWN_MS)
}

/// `retryAtFromHeader`: a `Retry-After` value (seconds or an HTTP date) as an epoch time.
pub fn retry_at_from_header(value: Option<&str>, now: i64) -> Option<i64> {
    let normalized = js_trim(value?);
    if !normalized.is_empty() && normalized.bytes().all(|b| b.is_ascii_digit()) {
        let seconds: f64 = normalized.parse().ok()?;
        let retry_at = now as f64 + seconds * 1_000.0;
        return (retry_at.fract() == 0.0 && retry_at.abs() <= MAX_SAFE_INTEGER).then_some(retry_at as i64);
    }
    let retry_at = date_parse_millis(normalized)?;
    (retry_at > now).then_some(retry_at)
}

/// The `SourceControlRateLimit` service.
#[derive(Clone)]
pub struct SourceControlRateLimit {
    entries: Arc<Mutex<HashMap<String, Entry>>>,
    clock: SharedClock,
}

impl SourceControlRateLimit {
    pub fn new(clock: SharedClock) -> Self {
        Self {
            entries: Arc::default(),
            clock,
        }
    }

    /// `check(key, {allowPaused})` in the current credential scope: the lease, or the pause.
    pub fn check(&self, key: &RateLimitKey, allow_paused: bool) -> Result<u64, SourceControlRateLimitPausedError> {
        self.check_in(&current_credential_scope(), key, allow_paused)
    }

    /// `check` in an explicit credential scope.
    pub fn check_in(&self, scope: &str, key: &RateLimitKey, allow_paused: bool) -> Result<u64, SourceControlRateLimitPausedError> {
        let now = self.clock.now_millis();
        let entries = self.entries.lock().expect("rate limit lock");
        let entry = entries.get(&normalized_key(key, scope)).copied();
        if let Some(entry) = entry {
            if entry.retry_at > now && !allow_paused {
                return Err(SourceControlRateLimitPausedError {
                    provider: key.provider,
                    host: js_trim(&key.host).to_lowercase(),
                    retry_at: entry.retry_at,
                });
            }
        }
        Ok(entry.map_or(0, |entry| entry.generation))
    }

    /// `recordRateLimit({...key, lease, retryAt})` in the current credential scope.
    pub fn record_rate_limit(&self, key: &RateLimitKey, lease: u64, retry_at: Option<i64>) {
        self.record_rate_limit_in(&current_credential_scope(), key, lease, retry_at);
    }

    pub fn record_rate_limit_in(&self, scope: &str, key: &RateLimitKey, lease: u64, retry_at: Option<i64>) {
        let now = self.clock.now_millis();
        let mut entries = self.entries.lock().expect("rate limit lock");
        let map_key = normalized_key(key, scope);
        let previous = entries.get(&map_key).copied();
        if let Some(previous) = previous {
            if previous.generation > lease {
                if previous.retry_at <= now && retry_at.is_none_or(|at| at <= now) {
                    return;
                }
                let next_retry_at = match retry_at {
                    Some(at) if at > previous.retry_at => at,
                    _ => previous.retry_at,
                };
                if next_retry_at != previous.retry_at {
                    entries.insert(
                        map_key,
                        Entry {
                            retry_at: next_retry_at,
                            ..previous
                        },
                    );
                }
                return;
            }
        }
        let attempt = previous.map_or(0, |p| p.attempt) + 1;
        let proposed = match retry_at {
            Some(at) if at > now => at,
            _ => now + fallback_cooldown_ms(attempt),
        };
        let next_retry_at = match previous {
            Some(previous) if previous.retry_at > now => previous.retry_at.max(proposed),
            _ => proposed,
        };
        entries.insert(
            map_key,
            Entry {
                attempt,
                generation: previous.map_or(0, |p| p.generation).max(lease) + 1,
                retry_at: next_retry_at,
            },
        );
    }

    /// `recordSuccess({...key, lease})` in the current credential scope.
    pub fn record_success(&self, key: &RateLimitKey, lease: u64) {
        self.record_success_in(&current_credential_scope(), key, lease);
    }

    pub fn record_success_in(&self, scope: &str, key: &RateLimitKey, lease: u64) {
        let now = self.clock.now_millis();
        let mut entries = self.entries.lock().expect("rate limit lock");
        let map_key = normalized_key(key, scope);
        let Some(previous) = entries.get(&map_key).copied() else {
            return;
        };
        if previous.generation != lease || previous.retry_at > now {
            return;
        }
        entries.insert(
            map_key,
            Entry {
                attempt: 0,
                generation: previous.generation,
                retry_at: 0,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::ManualClock;

    fn github() -> RateLimitKey {
        RateLimitKey::new(SourceControlProviderKind::Github, "github.com")
    }

    fn limits(start: i64) -> (SourceControlRateLimit, Arc<ManualClock>) {
        let clock = ManualClock::new(start);
        (SourceControlRateLimit::new(clock.clone()), clock)
    }

    #[tokio::test]
    async fn isolates_cooldowns_for_verified_credentials_on_the_same_host() {
        let (limits, _) = limits(0);
        with_credential_scope("first", async { limits.record_rate_limit(&github(), 0, None) }).await;
        with_credential_scope("second", async { limits.check(&github(), false) }).await.unwrap();
        let error = with_credential_scope("first", async { limits.check(&github(), false) }).await.unwrap_err();
        assert_eq!(serde_json::to_value(&error).unwrap()["_tag"], "SourceControlRateLimitPausedError");
    }

    #[test]
    fn parses_retry_after_seconds_and_http_dates() {
        assert_eq!(retry_at_from_header(Some("120"), 1_000), Some(121_000));
        assert_eq!(retry_at_from_header(Some("Thu, 01 Jan 1970 00:02:01 GMT"), 1_000), Some(121_000));
        assert_eq!(retry_at_from_header(Some("later"), 1_000), None);
        assert_eq!(retry_at_from_header(None, 1_000), None);
    }

    #[test]
    fn backs_off_repeated_rate_limits_until_a_successful_request() {
        let (limits, clock) = limits(0);
        let first = limits.check(&github(), false).unwrap();
        limits.record_rate_limit(&github(), first, None);
        let pause = limits.check(&github(), false).unwrap_err();
        assert_eq!(pause.retry_at, 30_000);
        assert_eq!(pause.host, "github.com");
        assert_eq!(pause.detail(), "github requests to github.com are paused until the rate limit resets.");
        assert_eq!(pause.message(), pause.detail());

        clock.advance(30_000);
        let second = limits.check(&github(), false).unwrap();
        limits.record_rate_limit(&github(), second, None);
        assert_eq!(limits.check(&github(), false).unwrap_err().retry_at, 90_000);

        clock.advance(60_000);
        let success = limits.check(&github(), false).unwrap();
        limits.record_success(&github(), success);
        let reset = limits.check(&github(), false).unwrap();
        limits.record_rate_limit(&github(), reset, None);
        assert_eq!(limits.check(&github(), false).unwrap_err().retry_at, 120_000);
    }

    #[test]
    fn honors_a_provider_reset_time() {
        let (limits, clock) = limits(1_000);
        let lease = limits.check(&github(), false).unwrap();
        limits.record_rate_limit(&github(), lease, Some(121_000));
        assert_eq!(limits.check(&github(), false).unwrap_err().retry_at, 121_000);
        clock.advance(119_999);
        assert_eq!(limits.check(&github(), false).unwrap_err().retry_at, 121_000);
        clock.advance(1);
        assert_eq!(limits.check(&github(), false).unwrap(), 1);
    }

    #[test]
    fn lets_an_interactive_request_through_without_clearing_an_active_pause() {
        let (limits, _) = limits(1_000);
        let lease = limits.check(&github(), false).unwrap();
        limits.record_rate_limit(&github(), lease, Some(121_000));
        let interactive = limits.check(&github(), true).unwrap();
        limits.record_success(&github(), interactive);
        assert_eq!(interactive, 1);
        assert_eq!(limits.check(&github(), false).unwrap_err().retry_at, 121_000);
        limits.record_rate_limit(&github(), interactive, Some(61_000));
        assert_eq!(limits.check(&github(), false).unwrap_err().retry_at, 121_000);
    }

    #[test]
    fn keeps_providers_and_hosts_isolated() {
        let (limits, _) = limits(0);
        let lease = limits.check(&github(), false).unwrap();
        limits.record_rate_limit(&github(), lease, None);
        assert_eq!(
            limits
                .check(&RateLimitKey::new(SourceControlProviderKind::Gitlab, "github.com"), false)
                .unwrap(),
            0
        );
        assert_eq!(
            limits
                .check(&RateLimitKey::new(SourceControlProviderKind::Github, "github.example.com"), false)
                .unwrap(),
            0
        );
    }

    #[test]
    fn does_not_let_an_older_success_clear_a_concurrent_pause() {
        let (limits, _) = limits(0);
        let first = limits.check(&github(), false).unwrap();
        let concurrent = limits.check(&github(), false).unwrap();
        limits.record_rate_limit(&github(), first, None);
        limits.record_success(&github(), concurrent);
        assert_eq!(limits.check(&github(), false).unwrap_err().retry_at, 30_000);
    }

    #[test]
    fn keeps_a_fresh_provider_reset_from_an_older_request() {
        let (limits, clock) = limits(0);
        let stale = limits.check(&github(), false).unwrap();
        limits.record_rate_limit(&github(), stale, None);
        clock.advance(31_000);
        limits.record_rate_limit(&github(), stale, Some(61_000));
        assert_eq!(limits.check(&github(), false).unwrap_err().retry_at, 61_000);
    }
}
