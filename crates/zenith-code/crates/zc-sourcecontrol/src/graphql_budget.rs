//! `githubGraphQlBudget.ts`: the GitHub GraphQL point budget per host and credential.
//!
//! Every read query asks for `rateLimit { cost limit remaining resetAt }`; [`GitHubGraphQlBudget::observe`]
//! learns the balance from the answers, and [`GitHubGraphQlBudget::query`] reserves the expected
//! cost before a read goes out. Background reads stop at the last **10%** of the limit (the
//! reserve), interactive ones (`allow_reserve`) only at zero; both fail with the shared
//! [`SourceControlRateLimitPausedError`] until the reset time.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use zc_contracts::SourceControlProviderKind;

use crate::rate_limit::{current_credential_scope, SourceControlRateLimitPausedError};
use crate::util::{date_parse_millis, js_trim, js_trim_start, SharedClock};

pub const GRAPHQL_RESERVE_RATIO: f64 = 0.1;
pub const RATE_LIMIT_SELECTION: &str = "rateLimit { cost limit remaining resetAt }";

#[derive(Debug, Clone, Copy, PartialEq)]
struct Snapshot {
    cost: f64,
    limit: f64,
    remaining: f64,
    reset_at_ms: i64,
}

fn host_key(host: &str) -> String {
    js_trim(host).to_lowercase()
}

fn snapshot_from(raw: &str) -> Option<Snapshot> {
    let parsed: Value = serde_json::from_str(raw).ok()?;
    let rate_limit = parsed.get("data")?.get("rateLimit")?;
    if !rate_limit.is_object() {
        return None;
    }
    let number = |key: &str| rate_limit.get(key).and_then(Value::as_f64).filter(|n| n.is_finite());
    let cost = number("cost").filter(|n| *n >= 0.0)?;
    let limit = number("limit").filter(|n| *n > 0.0)?;
    let remaining = number("remaining").filter(|n| *n >= 0.0)?;
    let reset_at = rate_limit.get("resetAt")?.as_str()?;
    let reset_at_ms = date_parse_millis(reset_at)?;
    Some(Snapshot {
        cost,
        limit,
        remaining,
        reset_at_ms,
    })
}

fn is_read_operation(document: &str) -> bool {
    let operation = js_trim_start(document);
    operation.starts_with("query") || operation.starts_with('{')
}

/// Adds the rate-limit selection to a read query (`withRateLimit`).
pub fn with_rate_limit(document: &str) -> String {
    if !is_read_operation(document) {
        return document.to_owned();
    }
    match document.rfind('}') {
        Some(end) if !document.contains(RATE_LIMIT_SELECTION) => {
            format!("{}\n  {RATE_LIMIT_SELECTION}\n{}", &document[..end], &document[end..])
        }
        _ => document.to_owned(),
    }
}

/// The `GitHubGraphQlBudget` service.
#[derive(Clone)]
pub struct GitHubGraphQlBudget {
    snapshots: Arc<Mutex<HashMap<String, Snapshot>>>,
    clock: SharedClock,
}

impl GitHubGraphQlBudget {
    pub fn new(clock: SharedClock) -> Self {
        Self {
            snapshots: Arc::default(),
            clock,
        }
    }

    /// `query(host, document, {allowReserve})` in the current credential scope.
    pub fn query(&self, host: &str, document: &str, allow_reserve: bool) -> Result<String, SourceControlRateLimitPausedError> {
        self.query_in(&current_credential_scope(), host, document, allow_reserve)
    }

    pub fn query_in(&self, scope: &str, host: &str, document: &str, allow_reserve: bool) -> Result<String, SourceControlRateLimitPausedError> {
        if !is_read_operation(document) {
            return Ok(document.to_owned());
        }
        let now = self.clock.now_millis();
        let key = format!("{}\0{scope}", host_key(host));
        let mut snapshots = self.snapshots.lock().expect("budget lock");
        if let Some(snapshot) = snapshots.get(&key).copied() {
            if snapshot.reset_at_ms <= now {
                snapshots.remove(&key);
            } else {
                let remaining = snapshot.remaining - snapshot.cost.max(1.0);
                if remaining < 0.0 || (!allow_reserve && remaining < snapshot.limit * GRAPHQL_RESERVE_RATIO) {
                    return Err(SourceControlRateLimitPausedError {
                        provider: SourceControlProviderKind::Github,
                        host: host_key(host),
                        retry_at: snapshot.reset_at_ms,
                    });
                }
                snapshots.insert(key, Snapshot { remaining, ..snapshot });
            }
        }
        Ok(with_rate_limit(document))
    }

    /// `observe(host, raw)` in the current credential scope.
    pub fn observe(&self, host: &str, raw: &str) {
        self.observe_in(&current_credential_scope(), host, raw);
    }

    pub fn observe_in(&self, scope: &str, host: &str, raw: &str) {
        let Some(snapshot) = snapshot_from(raw) else {
            return;
        };
        let key = format!("{}\0{scope}", host_key(host));
        let mut snapshots = self.snapshots.lock().expect("budget lock");
        let previous = snapshots.get(&key).copied();
        // Concurrent reads can finish out of order. Quota only falls within one reset window, and
        // an answer from an older window must not replace the current one.
        if let Some(previous) = previous {
            if snapshot.reset_at_ms < previous.reset_at_ms {
                return;
            }
        }
        // Keep the conservative balance, but learn the observed cost even when our reservation
        // was larger.
        let next = match previous {
            Some(previous) if snapshot.reset_at_ms == previous.reset_at_ms && snapshot.remaining >= previous.remaining => Snapshot {
                cost: snapshot.cost,
                ..previous
            },
            _ => snapshot,
        };
        snapshots.insert(key, next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rate_limit::with_credential_scope;
    use crate::util::ManualClock;

    const RESET_AT: &str = "2026-08-13T14:00:00.000Z";
    const NEXT_RESET_AT: &str = "2026-08-13T15:00:00.000Z";

    fn ms(iso: &str) -> i64 {
        date_parse_millis(iso).unwrap()
    }

    fn rate_limit(remaining: f64, limit: f64, reset_at: &str, cost: f64) -> String {
        serde_json::json!({"data": {"viewer": {"login": "someone"}, "rateLimit": {"cost": cost, "limit": limit, "remaining": remaining, "resetAt": reset_at}}})
            .to_string()
    }

    fn rl(remaining: f64) -> String {
        rate_limit(remaining, 5_000.0, RESET_AT, 14.0)
    }

    fn budget() -> (GitHubGraphQlBudget, Arc<ManualClock>) {
        let clock = ManualClock::new(ms("2026-08-13T13:30:00.000Z"));
        (GitHubGraphQlBudget::new(clock.clone()), clock)
    }

    const QUERY: &str = "query { viewer { login } }";

    #[test]
    fn adds_rate_metadata_to_a_read_query() {
        let (budget, _) = budget();
        let query = budget
            .query("github.com", r#"query { repository(owner: "acme", name: "web") { name } }"#, false)
            .unwrap();
        assert!(query.contains(RATE_LIMIT_SELECTION));
        assert!(query.contains(r#"repository(owner: "acme", name: "web") { name }"#));
    }

    #[test]
    fn protects_the_last_ten_percent() {
        let (budget, clock) = budget();
        budget.observe("github.com", &rl(500.0));
        let error = budget.query("github.com", QUERY, false).unwrap_err();
        assert_eq!(error.retry_at, ms(RESET_AT));
        assert_eq!(error.host, "github.com");
        assert_eq!(error.message(), "github requests to github.com are paused until the rate limit resets.");
        clock.set(ms("2026-08-13T14:00:01.000Z"));
        assert!(budget.query("github.com", QUERY, false).unwrap().contains("rateLimit"));
    }

    #[test]
    fn keeps_hosts_isolated() {
        let (budget, _) = budget();
        budget.observe("github.com", &rl(0.0));
        assert!(budget.query("github.com", QUERY, false).is_err());
        assert!(budget.query("github.example.com", QUERY, false).is_ok());
    }

    #[tokio::test]
    async fn isolates_reservations_and_observations_by_credential() {
        let (budget, _) = budget();
        with_credential_scope("first", async { budget.observe("github.com", &rl(0.0)) }).await;
        with_credential_scope("second", async { budget.observe("github.com", &rl(5000.0)) }).await;
        with_credential_scope("second", async { budget.query("github.com", QUERY, false) })
            .await
            .unwrap();
        let error = with_credential_scope("first", async { budget.query("github.com", QUERY, false) })
            .await
            .unwrap_err();
        assert_eq!(error.retry_at, ms(RESET_AT));
    }

    #[test]
    fn keeps_the_lower_remaining_value_from_out_of_order_responses() {
        let (budget, _) = budget();
        budget.observe("github.com", &rl(400.0));
        budget.observe("github.com", &rl(600.0));
        assert_eq!(budget.query("github.com", QUERY, false).unwrap_err().retry_at, ms(RESET_AT));
    }

    #[test]
    fn learns_a_cheaper_observed_cost_without_restoring_reserved_quota() {
        let (budget, _) = budget();
        budget.observe("github.com", &rate_limit(512.0, 5_000.0, RESET_AT, 8.0));
        budget.query("github.com", QUERY, false).unwrap();
        budget.observe("github.com", &rate_limit(511.0, 5_000.0, RESET_AT, 1.0));
        for _ in 0..4 {
            assert!(budget.query("github.com", QUERY, false).unwrap().contains("rateLimit"));
        }
        assert_eq!(budget.query("github.com", QUERY, false).unwrap_err().retry_at, ms(RESET_AT));
    }

    #[test]
    fn ignores_a_response_from_an_older_reset_window() {
        let (budget, _) = budget();
        budget.observe("github.com", &rate_limit(400.0, 5_000.0, NEXT_RESET_AT, 14.0));
        budget.observe("github.com", &rate_limit(1_000.0, 5_000.0, RESET_AT, 14.0));
        assert_eq!(budget.query("github.com", QUERY, false).unwrap_err().retry_at, ms(NEXT_RESET_AT));
    }

    #[test]
    fn accepts_a_response_from_a_later_reset_window() {
        let (budget, _) = budget();
        budget.observe("github.com", &rl(1_000.0));
        budget.observe("github.com", &rate_limit(400.0, 5_000.0, NEXT_RESET_AT, 14.0));
        assert_eq!(budget.query("github.com", QUERY, false).unwrap_err().retry_at, ms(NEXT_RESET_AT));
    }

    #[test]
    fn allows_reads_above_the_reserve_and_interactive_reads_in_it() {
        let (budget, _) = budget();
        budget.observe("github.com", &rl(515.0));
        assert!(budget.query("github.com", QUERY, false).is_ok());
        let (budget, _) = self::budget();
        budget.observe("github.com", &rl(500.0));
        assert!(budget.query("github.com", QUERY, true).is_ok());
    }

    #[test]
    fn reserves_an_admitted_query_cost_before_its_response() {
        let (budget, _) = budget();
        budget.observe("github.com", &rl(514.0));
        assert!(budget.query("github.com", QUERY, false).is_ok());
        assert_eq!(budget.query("github.com", QUERY, false).unwrap_err().retry_at, ms(RESET_AT));
    }

    #[test]
    fn stops_interactive_reads_when_the_reserve_is_exhausted() {
        let (budget, _) = budget();
        budget.observe("github.com", &rate_limit(1.0, 5000.0, RESET_AT, 1.0));
        budget.query("github.com", QUERY, true).unwrap();
        assert_eq!(budget.query("github.com", QUERY, true).unwrap_err().retry_at, ms(RESET_AT));
    }

    #[test]
    fn ignores_malformed_or_partial_rate_metadata() {
        let (budget, _) = budget();
        budget.observe("github.com", "{");
        budget.observe("github.com", r#"{"data":{"rateLimit":{"limit":0,"remaining":-1,"resetAt":"never"}}}"#);
        assert!(budget.query("github.com", QUERY, false).is_ok());
    }

    #[test]
    fn does_not_add_a_read_field_to_a_mutation() {
        let (budget, _) = budget();
        let mutation = "mutation { addComment(input: {}) { clientMutationId } }";
        budget.observe("github.com", &rl(0.0));
        assert_eq!(budget.query("github.com", mutation, false).unwrap(), mutation);
    }
}
