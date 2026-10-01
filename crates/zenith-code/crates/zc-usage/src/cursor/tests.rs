//! `cursorUsageReader.test.ts` and the Cursor cases of `usageTranscriptReader.test.ts`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use serde_json::{json, Value};

use super::*;

type Handler = Box<dyn Fn(&HashMap<String, String>, &Value) -> HttpReply + Send + Sync>;
/// One recorded request: lower-cased headers, body, timeout.
type Request = (HashMap<String, String>, Value, Duration);

/// A scripted dashboard: records headers and bodies, answers through `handler`.
struct FakeHttp {
    handler: Handler,
    requests: Mutex<Vec<Request>>,
}

impl FakeHttp {
    fn new(handler: impl Fn(&HashMap<String, String>, &Value) -> HttpReply + Send + Sync + 'static) -> Self {
        Self {
            handler: Box::new(handler),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

#[async_trait]
impl CursorHttp for FakeHttp {
    async fn post(&self, url: &str, headers: &[(&str, String)], body: String, timeout: Duration) -> Result<HttpReply, String> {
        assert_eq!(url, CURSOR_USAGE_URL);
        let headers: HashMap<String, String> = headers.iter().map(|(name, value)| (name.to_lowercase(), value.clone())).collect();
        let body: Value = serde_json::from_str(&body).unwrap();
        let reply = (self.handler)(&headers, &body);
        self.requests.lock().unwrap().push((headers, body, timeout));
        Ok(reply)
    }
}

fn reply(body: Value) -> HttpReply {
    HttpReply {
        status: 200,
        body: body.to_string().into_bytes(),
    }
}

fn token(claims: Value) -> String {
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
    format!("header.{payload}.signature")
}

fn auth_file(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("auth.json");
    std::fs::write(
        &path,
        json!({"accessToken": token(json!({"sub": "auth|demo", "exp": 4_102_444_800u64}))}).to_string(),
    )
    .unwrap();
    path
}

struct FixedKeychain(Result<Option<String>, KeychainError>, AtomicUsize);

#[async_trait]
impl KeychainToken for FixedKeychain {
    async fn read(&self) -> Result<Option<String>, KeychainError> {
        self.1.fetch_add(1, Ordering::SeqCst);
        self.0.clone()
    }
}

#[tokio::test]
async fn keychain_login_reads_account_history() {
    let keychain = FixedKeychain(Ok(Some(token(json!({"sub": "auth|demo"})))), AtomicUsize::new(0));
    let http = FakeHttp::new(|headers, _| {
        assert!(headers["cookie"].contains("demo%3A%3A"));
        reply(json!({"totalUsageEventsCount": 0, "usageEventsDisplay": []}))
    });
    let result = read_cursor_account_usage(CredentialSource::Keychain(&keychain), 0.0, 1_781_000_000_000.0, &http).await;
    assert_eq!(keychain.1.load(Ordering::SeqCst), 1);
    assert_eq!(result.error, None);
    assert!(!result.missing);
    assert!(result.account_key.is_some());
}

#[tokio::test]
async fn unanswered_keychain_prompt_asks_for_approval() {
    let keychain = FixedKeychain(Err(KeychainError::Timeout), AtomicUsize::new(0));
    let http = FakeHttp::new(|_, _| panic!("no network expected"));
    let result = read_cursor_account_usage(CredentialSource::Keychain(&keychain), 0.0, 1.0, &http).await;
    assert_eq!(
        result,
        CursorAccountUsage {
            account_key: None,
            records: Vec::new(),
            missing: false,
            error: Some("Allow Keychain access on the Mac running zenith, then refresh.".into()),
        }
    );
}

#[tokio::test]
async fn paginated_history_with_headless_calls_and_cache_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let path = auth_file(dir.path());
    let http = FakeHttp::new(|headers, body| {
        assert_eq!(headers["origin"], "https://cursor.com");
        assert!(headers["cookie"].contains("WorkosCursorSessionToken=demo%3A%3A"));
        let page = body["page"].as_u64().unwrap();
        let events: Vec<Value> = (0..if page == 1 { 1000 } else { 1 })
            .map(|index| {
                json!({
                    "timestamp": (1_780_000_000_000u64 + ((page - 1) * 1000 + index) * 1000).to_string(),
                    "model": "claude-sonnet-4-5",
                    "conversationId": format!("conversation-{page}"),
                    "isHeadless": page == 2,
                    "chargedCents": 0,
                    "tokenUsage": {"inputTokens": 10, "outputTokens": 5, "cacheReadTokens": 30, "cacheWriteTokens": 2, "totalCents": 25},
                })
            })
            .collect();
        reply(json!({"totalUsageEventsCount": 1001, "usageEventsDisplay": events}))
    });
    let result = read_cursor_account_usage(CredentialSource::File(&path), 0.0, 1_781_000_000_000.0, &http).await;
    assert_eq!(result.error, None);
    let requests = http.requests.lock().unwrap();
    assert_eq!(requests.iter().map(|(_, body, _)| body["page"].as_u64().unwrap()).collect::<Vec<_>>(), [1, 2]);
    assert!(requests
        .iter()
        .all(|(_, body, timeout)| body["startDate"] == "0" && body["endDate"] == "1781000000000" && *timeout <= REQUEST_TIMEOUT));
    assert_eq!(result.records.len(), 1001);
    assert_eq!(&*result.records.last().unwrap().session_id, "conversation-2");
    assert_eq!(
        result.records[0].totals,
        Totals {
            uncached_input_tokens: 10.0,
            cached_input_tokens: 30.0,
            cache_creation_tokens: 2.0,
            output_tokens: 5.0,
            reasoning_tokens: 0.0
        }
    );
    assert_eq!(result.records[0].reported_cost_usd, Some(0.25));
    assert_eq!(result.records[0].rate_model.as_deref(), Some("claude-sonnet-4-5"));
    assert!(!result.account_key.unwrap().contains("demo"));
}

#[tokio::test]
async fn history_beyond_one_hundred_pages() {
    let dir = tempfile::tempdir().unwrap();
    let path = auth_file(dir.path());
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = requests.clone();
    let http = FakeHttp::new(move |_, _| {
        let request = counter.fetch_add(1, Ordering::SeqCst) + 1;
        let events: Vec<Value> = (0..if request <= 100 { 1000 } else { 1 }).map(|_| json!({"tokenUsage": null})).collect();
        reply(json!({"totalUsageEventsCount": 100_001, "usageEventsDisplay": events}))
    });
    let result = read_cursor_account_usage(CredentialSource::File(&path), 0.0, 1_781_000_000_000.0, &http).await;
    assert_eq!(result.error, None);
    assert_eq!(requests.load(Ordering::SeqCst), 101);
    assert!(result.records.is_empty());
}

#[tokio::test]
async fn confirmed_empty_history_versus_error_envelopes() {
    let dir = tempfile::tempdir().unwrap();
    let path = auth_file(dir.path());
    for body in [
        json!({}),
        json!({"totalUsageEventsCount": 0}),
        json!({"totalUsageEventsCount": 0, "usageEventsDisplay": []}),
    ] {
        let http = FakeHttp::new(move |_, _| reply(body.clone()));
        let result = read_cursor_account_usage(CredentialSource::File(&path), 0.0, 1_781_000_000_000.0, &http).await;
        assert_eq!(result.error, None);
        assert!(result.records.is_empty());
        assert!(!result.missing);
    }
    for body in [
        json!({"error": "upstream error"}),
        json!({"detail": "unknown error envelope"}),
        json!({"totalUsageEventsCount": 0, "error": "upstream error"}),
        Value::Null,
        json!([]),
        json!("invalid"),
        json!(0),
    ] {
        let http = FakeHttp::new(move |_, _| reply(body.clone()));
        let result = read_cursor_account_usage(CredentialSource::File(&path), 0.0, 1_781_000_000_000.0, &http).await;
        assert!(result.error.is_some());
        assert!(result.records.is_empty());
    }
}

#[tokio::test]
async fn a_full_page_at_the_reported_count_needs_a_terminal_page() {
    let dir = tempfile::tempdir().unwrap();
    let path = auth_file(dir.path());
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = requests.clone();
    let http = FakeHttp::new(move |_, _| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            let events: Vec<Value> = (0..1000u64)
                .map(|index| json!({"timestamp": (1_780_000_000_000u64 + index).to_string(), "model": "gpt-5", "tokenUsage": {"inputTokens": 10, "outputTokens": 5}}))
                .collect();
            reply(json!({"totalUsageEventsCount": 1000, "usageEventsDisplay": events}))
        } else {
            reply(json!({"totalUsageEventsCount": 1000}))
        }
    });
    let result = read_cursor_account_usage(CredentialSource::File(&path), 0.0, 1_781_000_000_000.0, &http).await;
    assert_eq!(result.error, None);
    assert_eq!(result.records.len(), 1000);
    assert_eq!(requests.load(Ordering::SeqCst), 2);
}

fn event(index: u64) -> Value {
    json!({"timestamp": (1_780_000_000_000u64 + index).to_string(), "model": "gpt-5", "tokenUsage": {"inputTokens": 10, "outputTokens": 5, "totalCents": 1}})
}

#[tokio::test]
async fn removes_only_count_proven_boundary_copies() {
    let dir = tempfile::tempdir().unwrap();
    let path = auth_file(dir.path());
    for total in [2000u64, 2001] {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = requests.clone();
        let http = FakeHttp::new(move |_, _| {
            let request = counter.fetch_add(1, Ordering::SeqCst) + 1;
            let events: Vec<Value> = match request {
                1 => (0..1000).map(event).collect(),
                2 => (0..1000).map(|index| event(999 + index)).collect(),
                _ => vec![event(1999)],
            };
            reply(json!({"totalUsageEventsCount": total, "usageEventsDisplay": events}))
        });
        let result = read_cursor_account_usage(CredentialSource::File(&path), 0.0, 1_781_000_000_000.0, &http).await;
        assert_eq!(result.error, None);
        assert_eq!(result.records.len() as u64, total);
        assert_eq!(requests.load(Ordering::SeqCst), 3);
        assert_eq!(result.records.last().unwrap().timestamp_ms, 1_780_000_001_999.0);
        assert_eq!(
            result.records.iter().filter(|record| record.timestamp_ms == 1_780_000_000_999.0).count(),
            if total == 2000 { 1 } else { 2 }
        );
        let keys: std::collections::HashSet<_> = result.records.iter().map(|record| record.dedupe_key.clone()).collect();
        assert_eq!(keys.len() as u64, total);
    }
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = requests.clone();
    let http = FakeHttp::new(move |_, _| {
        let events: Vec<Value> = if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            (0..1000).map(event).collect()
        } else {
            vec![event(500), event(1000)]
        };
        reply(json!({"totalUsageEventsCount": 1001, "usageEventsDisplay": events}))
    });
    let inconsistent = read_cursor_account_usage(CredentialSource::File(&path), 0.0, 1_781_000_000_000.0, &http).await;
    assert!(inconsistent.error.is_some());
    assert!(inconsistent.records.is_empty());
}

#[tokio::test]
async fn truncated_pages_and_denied_logins_are_not_complete_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = auth_file(dir.path());
    let http = FakeHttp::new(|_, _| reply(json!({"totalUsageEventsCount": 101, "usageEventsDisplay": []})));
    let truncated = read_cursor_account_usage(CredentialSource::File(&path), 0.0, 1_781_000_000_000.0, &http).await;
    assert!(truncated.error.is_some());
    assert!(truncated.records.is_empty());
    let secret = token(json!({"sub": "auth|demo", "exp": 4_102_444_800u64}));
    let body = secret.clone().into_bytes();
    let http = FakeHttp::new(move |_, _| HttpReply {
        status: 401,
        body: body.clone(),
    });
    let denied = read_cursor_account_usage(CredentialSource::File(&path), 0.0, 1_781_000_000_000.0, &http).await;
    assert!(!denied.error.as_deref().unwrap().contains(&secret));
    assert!(denied.records.is_empty());
    let http = FakeHttp::new(|_, _| reply(json!({})));
    let missing = read_cursor_account_usage(CredentialSource::File(&dir.path().join("missing.json")), 0.0, 1_781_000_000_000.0, &http).await;
    assert!(missing.missing);
    assert_eq!(http.count(), 0);
}

#[test]
fn rate_models_drop_cursor_tiers() {
    assert_eq!(cursor_rate_model("claude-fable-5-1-thinking-high"), "claude-fable-5-1");
    assert_eq!(cursor_rate_model("cursor-grok-4.7-high-fast"), "xai/grok-4.7");
    assert_eq!(cursor_rate_model("grok-4.7-xhigh-fast"), "xai/grok-4.7");
    assert_eq!(cursor_rate_model("default"), "default");
}

#[tokio::test]
async fn keychain_reads_are_shared_cached_and_outlive_a_timeout() {
    let reads = Arc::new(AtomicUsize::new(0));
    let (release, gate) = tokio::sync::watch::channel(false);
    let counter = reads.clone();
    let reader = CachedKeychainToken::new(
        Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            let mut gate = gate.clone();
            async move {
                let _ = gate.wait_for(|open| *open).await;
                Ok(Some("token".to_owned()))
            }
            .boxed()
        }),
        Duration::from_millis(50),
    );
    // Nobody answers in time: both callers give up, one read stays in flight.
    assert_eq!(reader.read().await, Err(KeychainError::Timeout));
    assert_eq!(reader.read().await, Err(KeychainError::Timeout));
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    release.send(true).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    // The late answer is cached.
    assert_eq!(reader.read().await, Ok(Some("token".to_owned())));
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}
