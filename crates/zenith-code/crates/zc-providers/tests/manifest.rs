//! Ports of the `ModelManifest service` cases of `ModelManifest.test.ts`, with a scripted fetcher
//! and a pinned clock.

use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};
use zc_providers::manifest::{bundled_model_manifest, encode_manifest_cache, ManifestFetcher, ModelManifest, ModelManifestData, ModelManifestOptions};

const REMOTE_UPDATED_AT: &str = "2099-01-01T00:00:00Z";

fn remote_manifest() -> Value {
    json!({"version": 1, "updatedAt": REMOTE_UPDATED_AT, "currentModels": {"codex": ["remote-model"], "claudeAgent": ["remote-agent-model"]}})
}

fn remote_claude_manifest() -> Value {
    json!({
        "version": 1, "updatedAt": REMOTE_UPDATED_AT, "currentModels": {},
        "providers": {"claudeAgent": {
            "profiles": {"synthetic": {"adapter": {"claudeCode": {"effortMap": {"extreme": "high"}}}}},
            "models": [{"slug": "remote-only-model", "name": "Remote Only Model", "status": "current", "profile": "synthetic"}]
        }}
    })
}

fn decoded(value: Value) -> ModelManifestData {
    ModelManifestData::decode(value).unwrap()
}

/// Answers from a script: `Err` for a failed fetch (HTTP error, timeout).
struct Scripted {
    fetches: AtomicUsize,
    respond: Box<dyn Fn(usize) -> Result<Value, String> + Send + Sync>,
}

#[async_trait]
impl ManifestFetcher for Scripted {
    async fn fetch(&self, _url: &str) -> Result<Value, String> {
        let index = self.fetches.fetch_add(1, Ordering::SeqCst);
        (self.respond)(index)
    }
}

struct Env {
    dir: tempfile::TempDir,
    clock: Arc<AtomicI64>,
    fetcher: Arc<Scripted>,
    checks_enabled: bool,
}

impl Env {
    fn new(respond: impl Fn(usize) -> Result<Value, String> + Send + Sync + 'static) -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            clock: Arc::new(AtomicI64::new(1_800_000_000_000)),
            fetcher: Arc::new(Scripted {
                fetches: AtomicUsize::new(0),
                respond: Box::new(respond),
            }),
            checks_enabled: true,
        }
    }

    fn cache_path(&self) -> std::path::PathBuf {
        self.dir.path().join("model-manifest.json")
    }

    fn make(&self) -> ModelManifest {
        let mut options = ModelManifestOptions::new(Some(self.cache_path()));
        options.fetcher = self.fetcher.clone();
        let clock = self.clock.clone();
        options.clock = Arc::new(move || clock.load(Ordering::SeqCst));
        let enabled = self.checks_enabled;
        options.update_checks_enabled = Some(Arc::new(move || Box::pin(async move { enabled })));
        ModelManifest::new(options)
    }

    fn fetches(&self) -> usize {
        self.fetcher.fetches.load(Ordering::SeqCst)
    }

    fn advance(&self, ms: i64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn explicit_refresh_bypasses_fresh_memory_and_disk_caches() {
    let updated = json!({"version": 1, "updatedAt": REMOTE_UPDATED_AT, "currentModels": {"codex": ["gpt-reloaded"]}});
    let env = Env::new({
        let updated = updated.clone();
        move |index| Ok(if index == 0 { remote_manifest() } else { updated.clone() })
    });
    let service = env.make();
    assert_eq!(*service.refresh().await, decoded(remote_manifest()));
    assert_eq!(*service.refresh().await, decoded(remote_manifest()));
    assert_eq!(env.fetches(), 1);
    let rebooted = env.make();
    assert_eq!(*rebooted.refresh().await, decoded(remote_manifest()));
    assert_eq!(env.fetches(), 1);
    assert_eq!(*rebooted.force_refresh().await, decoded(updated.clone()));
    assert_eq!(env.fetches(), 2);
    assert_eq!(*rebooted.current().await, decoded(updated.clone()));
    assert_eq!(*env.make().current().await, decoded(updated));
}

#[tokio::test]
async fn explicit_refresh_retries_after_a_failure_and_keeps_last_good_data() {
    let env = Env::new(|index| if index == 1 { Err("status 503".into()) } else { Ok(remote_manifest()) });
    let service = env.make();
    assert_eq!(*service.refresh().await, decoded(remote_manifest()));
    assert_eq!(*service.force_refresh().await, decoded(remote_manifest()));
    assert_eq!(*service.current().await, decoded(remote_manifest()));
    assert_eq!(*env.make().current().await, decoded(remote_manifest()));
    assert_eq!(env.fetches(), 2);
    assert_eq!(*service.force_refresh().await, decoded(remote_manifest()));
    assert_eq!(env.fetches(), 3);
}

#[tokio::test]
async fn explicit_refresh_bypasses_the_retry_delay_after_an_initial_failure() {
    let env = Env::new(|index| if index == 0 { Err("status 503".into()) } else { Ok(remote_manifest()) });
    let service = env.make();
    assert_eq!(*service.refresh().await, *bundled_model_manifest());
    assert_eq!(*service.refresh().await, *bundled_model_manifest());
    assert_eq!(env.fetches(), 1);
    assert_eq!(*service.force_refresh().await, decoded(remote_manifest()));
    assert_eq!(env.fetches(), 2);
}

#[tokio::test]
async fn ignores_older_remote_edits() {
    let remote = Arc::new(Mutex::new(
        json!({"version": 1, "updatedAt": "2000-01-01T00:00:00Z", "currentModels": {"codex": ["remote-model"], "claudeAgent": ["remote-agent-model"]}}),
    ));
    let env = Env::new({
        let remote = remote.clone();
        move |_| Ok(remote.lock().unwrap().clone())
    });
    let service = env.make();
    assert_eq!(*service.refresh().await, *bundled_model_manifest());
    assert_eq!(*service.current().await, *bundled_model_manifest());
    *remote.lock().unwrap() = remote_manifest();
    assert_eq!(*service.force_refresh().await, decoded(remote_manifest()));
    let mut same_as_bundle = remote_manifest();
    same_as_bundle["updatedAt"] = json!(bundled_model_manifest().updated_at.clone());
    *remote.lock().unwrap() = same_as_bundle;
    assert_eq!(*service.force_refresh().await, decoded(remote_manifest()));
    assert_eq!(*env.make().current().await, decoded(remote_manifest()));
}

#[tokio::test]
async fn malformed_payloads_keep_the_bundle() {
    let env = Env::new(|_| Ok(json!({"version": 999, "nonsense": true})));
    assert_eq!(*env.make().refresh().await, *bundled_model_manifest());
}

#[tokio::test]
async fn invalid_later_payloads_keep_the_last_good_cache() {
    let mut invalid: Vec<Value> = Vec::new();
    let mut bad_effort = remote_claude_manifest();
    bad_effort["providers"]["claudeAgent"]["profiles"]["synthetic"]["adapter"]["claudeCode"]["effortMap"]["extreme"] = json!(123);
    invalid.push(bad_effort);
    let mut missing_profile = remote_claude_manifest();
    missing_profile["providers"]["claudeAgent"]["profiles"] = json!({});
    invalid.push(missing_profile);
    let mut duplicate = remote_claude_manifest();
    duplicate["providers"]["claudeAgent"]["models"]
        .as_array_mut()
        .unwrap()
        .push(json!({"slug": "remote-only-model", "name": "Duplicate", "status": "current", "profile": "synthetic"}));
    invalid.push(duplicate);
    let mut absent_default = remote_claude_manifest();
    absent_default["providers"]["claudeAgent"]["defaults"] = json!({"chat": "absent-model"});
    invalid.push(absent_default);
    for compat in [
        json!({"minVersion": "2.x"}),
        json!({"maxVersionExclusive": "2.x"}),
        json!({"minVersion": "2.2", "maxVersionExclusive": "2.1"}),
    ] {
        let mut bad = remote_claude_manifest();
        bad["providers"]["claudeAgent"]["models"][0]["adapter"] = json!({"claudeCode": compat});
        invalid.push(bad);
    }
    let responses: Vec<Value> = std::iter::once(remote_claude_manifest()).chain(invalid.clone()).collect();
    let env = Env::new(move |index| Ok(responses[index.min(responses.len() - 1)].clone()));
    let service = env.make();
    assert_eq!(*service.refresh().await, decoded(remote_claude_manifest()));
    for _ in &invalid {
        env.advance(60 * 60 * 1000);
        assert_eq!(*service.refresh().await, decoded(remote_claude_manifest()));
    }
    assert_eq!(env.fetches(), invalid.len() + 1);
    assert_eq!(*env.make().current().await, decoded(remote_claude_manifest()));
}

#[tokio::test]
async fn drops_a_disk_cache_older_than_the_bundle() {
    let env = Env::new(|_| Ok(remote_manifest()));
    let mut undated = decoded(remote_manifest());
    undated.updated_at = None;
    let mut old = decoded(remote_manifest());
    old.updated_at = Some("2000-01-01T00:00:00Z".into());
    for stale in [undated, old] {
        std::fs::write(env.cache_path(), encode_manifest_cache(0, &stale)).unwrap();
        assert_eq!(*env.make().current().await, *bundled_model_manifest());
    }
    std::fs::write(env.cache_path(), encode_manifest_cache(0, &decoded(remote_manifest()))).unwrap();
    assert_eq!(*env.make().current().await, decoded(remote_manifest()));
}

#[tokio::test]
async fn does_not_fetch_when_update_checks_are_disabled() {
    let mut env = Env::new(|_| Ok(remote_manifest()));
    env.checks_enabled = false;
    let service = env.make();
    assert_eq!(*service.refresh().await, *bundled_model_manifest());
    assert_eq!(*service.force_refresh().await, *bundled_model_manifest());
    assert_eq!(env.fetches(), 0);
}

#[tokio::test]
async fn caches_valid_compatibility_policies_and_keeps_them_after_a_malformed_refresh() {
    let mut remote = remote_manifest();
    remote["compatibility"] =
        json!([{"driver": "codex", "t3CodeRange": ">=0.0.42", "recommendedVersion": "2.0.0", "ranges": [{"range": "=2.0.0", "status": "supported"}]}]);
    let invalid = Arc::new(Mutex::new(false));
    let env = Env::new({
        let remote = remote.clone();
        let invalid = invalid.clone();
        move |_| {
            let mut value = remote.clone();
            if *invalid.lock().unwrap() {
                value["compatibility"][0]["recommendedVersion"] = json!("3.0.0");
            }
            Ok(value)
        }
    });
    let service = env.make();
    let expected = decoded(remote.clone()).compatibility;
    assert_eq!(service.refresh().await.compatibility, expected);
    *invalid.lock().unwrap() = true;
    env.advance(60 * 60 * 1000);
    assert_eq!(service.refresh().await.compatibility, expected);
    assert_eq!(env.make().current().await.compatibility, expected);
}
