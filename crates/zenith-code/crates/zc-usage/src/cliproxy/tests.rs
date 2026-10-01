//! `cliproxyApi.test.ts`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use serde_json::{json, Value};

use super::*;

fn config() -> SourceConfig {
    SourceConfig {
        kind: "cliproxy".into(),
        label: None,
        url: "http://hub.test:8317".into(),
        management_key: "management-secret".into(),
        enabled: true,
    }
}

fn accounts() -> Vec<Value> {
    vec![
        json!({"id": "first.json", "auth_index": "a", "provider": "codex", "email": "first@example.com", "id_token": {"chatgpt_account_id": "account-a"}}),
        json!({"id": "second.json", "auth_index": "b", "provider": "codex", "email": "second@example.com", "id_token": {"chatgpt_account_id": "account-b"}}),
    ]
}

fn credit(id: &str, expires_at: &str) -> Value {
    json!({"id": id, "expires_at": expires_at, "status": "available", "reset_type": "codex_rate_limits"})
}

type Upstream = Box<dyn Fn(&Value) -> (u16, Value) + Send + Sync>;

/// A hub: the management endpoints, with `api-call` answered by `upstream`.
struct FakeHub {
    accounts: Vec<Value>,
    upstream: Option<Upstream>,
    cooldown_status: u16,
    requests: Mutex<Vec<(String, Option<Value>)>>,
}

impl FakeHub {
    fn new() -> Self {
        Self {
            accounts: accounts(),
            upstream: None,
            cooldown_status: 200,
            requests: Mutex::new(Vec::new()),
        }
    }

    fn api(self: &Arc<Self>) -> CliproxyApi {
        CliproxyApi::new(self.clone(), Arc::new(|| 1_788_710_400_000.0))
    }

    fn requests(&self) -> Vec<(String, Option<Value>)> {
        self.requests.lock().unwrap().clone()
    }

    fn default_upstream(body: &Value) -> (u16, Value) {
        let url = body["url"].as_str().unwrap_or("");
        if url.ends_with("/consume") {
            (200, json!({"code": "reset"}))
        } else if url.ends_with("/rate-limit-reset-credits") {
            (
                200,
                json!({"credits": [
                    credit("later", "2099-02-01T00:00:00Z"),
                    credit("first", "2099-01-01T00:00:00Z"),
                    credit("expired", "2000-01-01T00:00:00Z"),
                    {"id": "used", "expires_at": "2099-01-01T00:00:00Z", "status": "redeemed", "reset_type": "codex_rate_limits"},
                ]}),
            )
        } else {
            (
                200,
                json!({"plan_type": "pro", "rate_limit": {"secondary_window": {"used_percent": 78, "reset_at": 4_070_908_800u64, "limit_window_seconds": 604_800}}}),
            )
        }
    }
}

#[async_trait]
impl HubHttp for FakeHub {
    async fn send(&self, url: &str, authorization: &str, json_body: Option<String>, _: Duration) -> Result<HubReply, String> {
        assert_eq!(authorization, "Bearer management-secret");
        let path = url::Url::parse(url).unwrap().path().to_owned();
        let body: Option<Value> = json_body.map(|body| serde_json::from_str(&body).unwrap());
        self.requests.lock().unwrap().push((path.clone(), body.clone()));
        let reply = |status: u16, body: Value| {
            Ok(HubReply {
                status,
                body: body.to_string().into_bytes(),
            })
        };
        if path.ends_with("/auth-files") {
            return reply(200, json!({"files": self.accounts}));
        }
        if path.ends_with("/reset-quota") {
            return reply(self.cooldown_status, json!({}));
        }
        assert_eq!(path, "/v0/management/api-call");
        let body = body.unwrap();
        assert_eq!(body["header"]["Authorization"], "Bearer $TOKEN$");
        let (status, upstream) = match &self.upstream {
            Some(upstream) => upstream(&body),
            None => Self::default_upstream(&body),
        };
        reply(200, json!({"status_code": status, "body": upstream.to_string()}))
    }
}

#[tokio::test]
async fn reads_both_accounts_and_their_earliest_unexpired_credits() {
    let hub = Arc::new(FakeHub::new());
    let result = hub.api().read_accounts(&config()).await.unwrap();
    let credits: Vec<&Value> = result.iter().map(|account| &account["usageLimits"]["resetCredits"]).collect();
    let expected = json!({"availableCount": 2, "nextCreditId": "first", "nextExpiresAt": "2099-01-01T00:00:00.000Z"});
    assert_eq!(credits, [&expected, &expected]);
    let window = &result[0]["usageLimits"]["windows"][0];
    assert_eq!(
        (window["id"].as_str(), window["usedPercent"].as_f64(), window["kind"].as_str()),
        (Some("secondary"), Some(78.0), Some("weekly"))
    );
    assert_eq!(result[0]["plan"], "ChatGPT Pro 20x Subscription");
    let calls: Vec<Value> = hub
        .requests()
        .into_iter()
        .filter_map(|(_, body)| body)
        .filter(|body| body.get("url").is_some())
        .collect();
    let mut indexes: Vec<&str> = calls.iter().map(|body| body["auth_index"].as_str().unwrap()).collect();
    indexes.sort_unstable();
    assert_eq!(indexes, ["a", "a", "b", "b"]);
    assert_eq!(
        calls.iter().find(|body| body["auth_index"] == "b").unwrap()["header"]["Chatgpt-Account-Id"],
        "account-b"
    );
}

#[tokio::test]
async fn keeps_usage_when_the_credits_endpoint_fails() {
    let mut hub = FakeHub::new();
    hub.upstream = Some(Box::new(|body| {
        if body["url"].as_str().unwrap().ends_with("rate-limit-reset-credits") {
            (503, json!({"token": "do-not-publish"}))
        } else {
            (200, json!({"rate_limit": {"primary_window": {"used_percent": 12}}}))
        }
    }));
    let result = Arc::new(hub).api().read_accounts(&config()).await.unwrap();
    assert_eq!(result[0]["usageLimits"]["windows"][0]["usedPercent"], 12);
    assert!(result[0]["usageLimits"].get("resetCredits").is_none());
}

#[tokio::test]
async fn isolates_a_failed_account_without_publishing_upstream_bodies() {
    let mut hub = FakeHub::new();
    hub.upstream = Some(Box::new(|body| {
        if body["auth_index"] == "a" {
            (401, json!({"token": "do-not-publish"}))
        } else if body["url"].as_str().unwrap().ends_with("rate-limit-reset-credits") {
            (200, json!({"credits": []}))
        } else {
            (200, json!({"rate_limit": {"primary_window": {"used_percent": 12}}}))
        }
    }));
    let result = Arc::new(hub).api().read_accounts(&config()).await.unwrap();
    assert_eq!(result[0]["usageLimits"]["unavailable"]["reason"], "probeFailed");
    assert_eq!(result[1]["usageLimits"]["windows"][0]["usedPercent"], 12);
    assert!(!Value::Array(result).to_string().contains("do-not-publish"));
}

#[tokio::test]
async fn maps_claude_scoped_windows() {
    let mut hub = FakeHub::new();
    let mut account = accounts()[0].clone();
    account["provider"] = json!("claude");
    hub.accounts = vec![account];
    hub.upstream = Some(Box::new(|_| {
        (
            200,
            json!({
                "five_hour": {"utilization": 10, "resets_at": null},
                "seven_day": {"utilization": 50, "resets_at": "2099-01-01T00:00:00Z"},
                "limits": [{"kind": "weekly_scoped", "percent": 80, "resets_at": null, "scope": {"model": {"display_name": "Fable"}}}],
            }),
        )
    }));
    let result = Arc::new(hub).api().read_accounts(&config()).await.unwrap();
    let windows: Vec<(String, f64)> = result[0]["usageLimits"]["windows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|window| (window["id"].as_str().unwrap().to_owned(), window["usedPercent"].as_f64().unwrap()))
        .collect();
    assert_eq!(
        windows,
        [
            ("five_hour".to_owned(), 10.0),
            ("seven_day".to_owned(), 50.0),
            ("seven_day_fable".to_owned(), 80.0)
        ]
    );
    assert_eq!(result[0]["driver"], "claudeAgent");
    assert_eq!(result[0]["plan"], "Claude Subscription");
}

#[tokio::test]
async fn pins_redemption_to_the_credit_and_clears_only_that_cooldown() {
    let hub = Arc::new(FakeHub::new());
    let api = hub.api();
    assert_eq!(api.consume(&config(), "second.json", "credit-b").await.unwrap(), json!({"outcome": "reset"}));
    assert_eq!(api.consume(&config(), "second.json", "credit-b").await.unwrap(), json!({"outcome": "reset"}));
    let requests = hub.requests();
    let redemptions: Vec<&Value> = requests
        .iter()
        .filter_map(|(_, body)| body.as_ref())
        .filter(|body| body["url"].as_str().is_some_and(|url| url.ends_with("/consume")))
        .collect();
    assert_eq!(redemptions.len(), 2);
    assert_eq!(redemptions[0]["data"], redemptions[1]["data"]);
    assert_eq!(
        redemptions[0]["data"],
        json!({"redeem_request_id": credit_redeem_request_id("account-b", "credit-b"), "credit_id": "credit-b"}).to_string()
    );
    let cooldowns: Vec<&Value> = requests
        .iter()
        .filter(|(path, _)| path.ends_with("/reset-quota"))
        .map(|(_, body)| &body.as_ref().unwrap()["auth_index"])
        .collect();
    assert_eq!(cooldowns, [&json!("b"), &json!("b")]);
}

#[tokio::test]
async fn reports_each_outcome() {
    for (code, outcome) in [
        ("nothing_to_reset", "nothingToReset"),
        ("no_credit", "noCredit"),
        ("already_redeemed", "alreadyRedeemed"),
    ] {
        let mut hub = FakeHub::new();
        hub.upstream = Some(Box::new(move |_| (200, json!({"code": code}))));
        let hub = Arc::new(hub);
        assert_eq!(hub.api().consume(&config(), "first.json", "credit").await.unwrap(), json!({"outcome": outcome}));
        assert_eq!(
            hub.requests().iter().any(|(path, _)| path.ends_with("/reset-quota")),
            code == "already_redeemed"
        );
    }
}

#[tokio::test]
async fn redemption_succeeds_when_the_cooldown_cannot_be_cleared() {
    let mut hub = FakeHub::new();
    hub.cooldown_status = 404;
    let result = Arc::new(hub).api().consume(&config(), "first.json", "credit").await.unwrap();
    assert_eq!(result["outcome"], "reset");
    assert!(result["warning"].as_str().unwrap().contains("cooldown"));
}

#[tokio::test]
async fn skips_disabled_accounts_and_refuses_to_redeem_on_them() {
    let mut hub = FakeHub::new();
    let mut account = accounts()[0].clone();
    account["disabled"] = json!(true);
    hub.accounts = vec![account];
    let hub = Arc::new(hub);
    assert!(hub.api().read_accounts(&config()).await.unwrap().is_empty());
    assert!(hub.api().consume(&config(), "first.json", "credit").await.is_err());
    assert!(hub.requests().iter().all(|(path, _)| path.ends_with("/auth-files")));
}

#[tokio::test]
async fn keeps_the_redemption_id_after_an_uncertain_failure() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    let mut hub = FakeHub::new();
    hub.upstream = Some(Box::new(move |_| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            (503, json!({}))
        } else {
            (200, json!({"code": "already_redeemed"}))
        }
    }));
    let hub = Arc::new(hub);
    assert!(hub.api().consume(&config(), "first.json", "credit").await.is_err());
    assert_eq!(
        hub.api().consume(&config(), "first.json", "credit").await.unwrap(),
        json!({"outcome": "alreadyRedeemed"})
    );
    let requests = hub.requests();
    let data: Vec<&Value> = requests
        .iter()
        .filter_map(|(_, body)| body.as_ref())
        .filter(|body| body["url"].as_str().is_some_and(|url| url.ends_with("/consume")))
        .map(|body| &body["data"])
        .collect();
    assert_eq!(data[0], data[1]);
    assert_eq!(requests.iter().filter(|(path, _)| path.ends_with("/reset-quota")).count(), 1);
}

#[tokio::test]
async fn rejects_unknown_accounts_without_forwarding() {
    let hub = Arc::new(FakeHub::new());
    assert!(hub.api().consume(&config(), "missing.json", "credit").await.is_err());
    assert_eq!(hub.requests().len(), 1);
}

#[test]
fn redemption_ids_are_uuid_v5_shaped() {
    // The TS implementation's value for the same input.
    assert_eq!(credit_redeem_request_id("account-b", "credit-b"), "519d5243-011a-5b7b-91f3-44d85f095705");
}
