//! `UsageLimitSources` (`usage/UsageLimitSources.ts`): quota from places this environment
//! cannot run turns on, today CLIProxyAPI hubs pooling subscription accounts.
//!
//! Each enabled `settings.usageLimitSources` entry is read on the provider health-check
//! interval and on every change of that setting, and published as one snapshot per source
//! (`usageLimitSourcesUpdated` on `subscribeServerConfig`). A failing source keeps its row
//! with `error` set. Nothing is persisted.

use std::sync::Arc;
use std::time::Duration;

use futures::future::{BoxFuture, FutureExt};
use futures::stream::{self, BoxStream, StreamExt};
use serde_json::{json, Map, Value};
use zc_core::pubsub::SnapshotHub;
use zc_settings::config::{ConfigEventSource, ConfigOptions};

use crate::cliproxy::{CliproxyApi, SourceConfig, SourceError};
use crate::settings::UsageSettings;
use crate::time::iso_from_millis;

/// Asks the background policy whether provider-status work may run now.
pub type ShouldRun = Arc<dyn Fn() -> BoxFuture<'static, bool> + Send + Sync>;

struct Inner {
    api: CliproxyApi,
    settings: Arc<dyn UsageSettings>,
    should_run: ShouldRun,
    now_ms: Arc<dyn Fn() -> f64 + Send + Sync>,
    hub: SnapshotHub<Arc<Vec<Value>>>,
    /// One refresh (or redemption) at a time, so a slow read cannot resurrect a removed hub.
    refresh_lock: tokio::sync::Mutex<()>,
    tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

/// The service; cheap to clone.
#[derive(Clone)]
pub struct UsageLimitSources {
    inner: Arc<Inner>,
}

/// `sourceLabel`: the configured label, else the URL's host.
fn source_label(id: &str, config: &SourceConfig) -> String {
    if let Some(label) = config.label.as_deref().filter(|label| !label.is_empty()) {
        return label.to_owned();
    }
    match url::Url::parse(&config.url) {
        Ok(url) => {
            let host = url.host_str().unwrap_or("").to_owned();
            match url.port() {
                Some(port) => format!("{host}:{port}"),
                None => host,
            }
        }
        Err(_) => id.to_owned(),
    }
}

impl UsageLimitSources {
    pub fn new(api: CliproxyApi, settings: Arc<dyn UsageSettings>, should_run: ShouldRun, now_ms: Arc<dyn Fn() -> f64 + Send + Sync>) -> Self {
        Self {
            inner: Arc::new(Inner {
                api,
                settings,
                should_run,
                now_ms,
                hub: SnapshotHub::new(Arc::new(Vec::new())),
                refresh_lock: tokio::sync::Mutex::new(()),
                tasks: std::sync::Mutex::new(Vec::new()),
            }),
        }
    }

    /// The current snapshots.
    pub fn current(&self) -> Arc<Vec<Value>> {
        self.inner.hub.latest()
    }

    /// The current set followed by every change, repeats dropped.
    pub fn stream_changes(&self) -> BoxStream<'static, Arc<Vec<Value>>> {
        let (latest, changes) = self.inner.hub.subscribe();
        let mut previous: Option<Arc<Vec<Value>>> = None;
        stream::once(async move { latest })
            .chain(changes)
            .filter(move |next| {
                let keep = previous.as_ref().is_none_or(|previous| previous != next);
                if keep {
                    previous = Some(next.clone());
                }
                futures::future::ready(keep)
            })
            .boxed()
    }

    async fn read_source(&self, id: &str, config: &SourceConfig) -> Value {
        let mut snapshot = Map::new();
        snapshot.insert("id".into(), json!(id));
        snapshot.insert("kind".into(), json!(config.kind));
        snapshot.insert("label".into(), json!(source_label(id, config)));
        snapshot.insert("checkedAt".into(), json!(iso_from_millis((self.inner.now_ms)()).unwrap_or_default()));
        if config.management_key.is_empty() {
            snapshot.insert("accounts".into(), json!([]));
            snapshot.insert("error".into(), json!("No management key configured."));
            return Value::Object(snapshot);
        }
        match self.inner.api.read_accounts(config).await {
            Ok(accounts) => {
                snapshot.insert("accounts".into(), Value::Array(accounts));
            }
            Err(error) => {
                tracing::debug!(id, detail = %error.0, "usage limit source read failed");
                snapshot.insert("accounts".into(), json!([]));
                snapshot.insert("error".into(), json!(error.0));
            }
        }
        Value::Object(snapshot)
    }

    fn publish(&self, next: Vec<Value>) {
        let next = Arc::new(next);
        self.inner.hub.update(|previous| (**previous != *next).then(|| next.clone()));
    }

    async fn refresh_locked(&self) {
        let settings = self.inner.settings.get().await.ok();
        let entries: Vec<(String, SourceConfig)> = settings
            .as_ref()
            .and_then(|settings| settings.get("usageLimitSources"))
            .and_then(Value::as_object)
            .map(|sources| {
                sources
                    .iter()
                    .filter_map(|(id, config)| SourceConfig::from_value(config).map(|config| (id.clone(), config)))
                    .filter(|(_, config)| config.enabled)
                    .collect()
            })
            .unwrap_or_default();
        let reads: Vec<BoxFuture<'_, Value>> = entries.iter().map(|(id, config)| self.read_source(id, config).boxed()).collect();
        let snapshots: Vec<Value> = stream::iter(reads).buffered(4).collect().await;
        self.publish(snapshots);
    }

    /// Re-reads every source now. Never fails; failures land on the snapshots.
    pub async fn refresh(&self) {
        let _permit = self.inner.refresh_lock.lock().await;
        self.refresh_locked().await;
    }

    /// `consumeResetCredit({sourceId, accountId, creditId})`: the encoded
    /// `ProviderConsumeResetCreditResult`, or a `UsageLimitSourceError`.
    pub async fn consume_reset_credit(&self, source_id: &str, account_id: &str, credit_id: &str) -> Result<Value, SourceError> {
        let _permit = self.inner.refresh_lock.lock().await;
        let settings = self
            .inner
            .settings
            .get()
            .await
            .map_err(|_| SourceError("Could not read hub settings.".to_owned()))?;
        let config = settings
            .get("usageLimitSources")
            .and_then(|sources| sources.get(source_id))
            .and_then(SourceConfig::from_value)
            .filter(|config| config.enabled && !config.management_key.is_empty())
            .ok_or_else(|| SourceError("The usage limit source is missing or disabled.".to_owned()))?;
        let result = self.inner.api.consume(&config, account_id, credit_id).await?;
        let snapshot = self.read_source(source_id, &config).await;
        let next: Vec<Value> = self
            .current()
            .iter()
            .map(|source| {
                if source.get("id").and_then(Value::as_str) == Some(source_id) {
                    snapshot.clone()
                } else {
                    source.clone()
                }
            })
            .collect();
        self.publish(next);
        Ok(result)
    }

    /// Starts the settings watcher, the interval and the first read (the layer's forks).
    pub fn start(&self) {
        let mut tasks = self.inner.tasks.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !tasks.is_empty() {
            return;
        }
        // Settings edits re-read straight away, so a new hub shows up and a removed one leaves.
        let watcher = self.clone();
        let mut changes = self.inner.settings.changes();
        tasks.push(tokio::spawn(async move {
            let mut previous: Option<Value> = None;
            while let Some(settings) = changes.next().await {
                let sources = settings.get("usageLimitSources").cloned().unwrap_or(Value::Null);
                if previous.as_ref() == Some(&sources) {
                    continue;
                }
                previous = Some(sources);
                watcher.refresh().await;
            }
        }));
        let ticker = self.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                let interval = match ticker.inner.settings.get().await {
                    Ok(settings) => zc_providers::settings::provider_health_refresh_interval_ms(&settings),
                    Err(_) => zc_providers::settings::DEFAULT_PROVIDER_HEALTH_REFRESH_INTERVAL_MS,
                };
                #[allow(clippy::cast_sign_loss)]
                let wait = if interval <= 0 {
                    Duration::from_secs(60)
                } else {
                    Duration::from_millis(interval as u64)
                };
                tokio::time::sleep(wait).await;
                if (ticker.inner.should_run)().await {
                    ticker.refresh().await;
                }
            }
        }));
        let first = self.clone();
        tasks.push(tokio::spawn(async move { first.refresh().await }));
    }

    /// Stops the background work.
    pub fn shutdown(&self) {
        let tasks = std::mem::take(&mut *self.inner.tasks.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        for task in tasks {
            task.abort();
        }
    }
}

/// `usageLimitSourcesUpdated` for subscribers that ask for it (`usageLimitSources: true`):
/// an older client would die on the unknown event.
pub struct UsageLimitSourcesEvents(pub UsageLimitSources);

impl ConfigEventSource for UsageLimitSourcesEvents {
    fn subscribe(&self, options: &ConfigOptions) -> Option<BoxStream<'static, Value>> {
        if !options.usage_limit_sources {
            return None;
        }
        Some(
            self.0
                .stream_changes()
                .map(|sources| json!({"version": 1, "type": "usageLimitSourcesUpdated", "payload": {"sources": *sources}}))
                .boxed(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::cliproxy::{HubHttp, HubReply};

    struct Settings(Mutex<Value>);

    #[async_trait]
    impl UsageSettings for Settings {
        async fn get(&self) -> Result<Value, String> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn changes(&self) -> BoxStream<'static, Value> {
            Box::pin(stream::pending())
        }
    }

    /// One Codex account; every upstream call answers with 12% used.
    struct Hub;

    #[async_trait]
    impl HubHttp for Hub {
        async fn send(&self, url: &str, _: &str, body: Option<String>, _: Duration) -> Result<HubReply, String> {
            let reply = |body: Value| {
                Ok(HubReply {
                    status: 200,
                    body: body.to_string().into_bytes(),
                })
            };
            if url.ends_with("/auth-files") {
                return reply(json!({"files": [{"id": "pooled.json", "auth_index": "a", "provider": "codex"}]}));
            }
            if url.ends_with("/reset-quota") {
                return reply(json!({}));
            }
            let request: Value = serde_json::from_str(&body.unwrap()).unwrap();
            let upstream = match request["url"].as_str().unwrap() {
                url if url.ends_with("/consume") => json!({"code": "reset"}),
                url if url.ends_with("/rate-limit-reset-credits") => json!({"credits": []}),
                _ => json!({"rate_limit": {"primary_window": {"used_percent": 12}}}),
            };
            reply(json!({"status_code": 200, "body": upstream.to_string()}))
        }
    }

    fn sources(settings: Value) -> UsageLimitSources {
        let now: Arc<dyn Fn() -> f64 + Send + Sync> = Arc::new(|| 1_788_710_400_000.0);
        UsageLimitSources::new(
            CliproxyApi::new(Arc::new(Hub), now.clone()),
            Arc::new(Settings(Mutex::new(settings))),
            Arc::new(|| futures::future::ready(true).boxed()),
            now,
        )
    }

    #[tokio::test]
    async fn refresh_publishes_one_snapshot_per_enabled_source() {
        let service = sources(json!({"usageLimitSources": {
            "hub": {"kind": "cliproxy", "url": "http://hub.test:8317", "managementKey": "key", "enabled": true},
            "keyless": {"kind": "cliproxy", "label": "Team hub", "url": "https://team.test", "managementKey": "", "enabled": true},
            "off": {"kind": "cliproxy", "url": "https://off.test", "managementKey": "key", "enabled": false},
        }}));
        let mut changes = service.stream_changes();
        assert_eq!(*changes.next().await.unwrap(), Vec::<Value>::new());
        service.refresh().await;
        let published = changes.next().await.unwrap();
        assert_eq!(published.len(), 2);
        assert_eq!(published[0]["label"], "hub.test:8317");
        assert_eq!(published[0]["checkedAt"], "2026-09-06T16:00:00.000Z");
        assert_eq!(published[0]["accounts"][0]["usageLimits"]["windows"][0]["usedPercent"], 12);
        assert!(published[0].get("error").is_none());
        assert_eq!(published[1]["label"], "Team hub");
        assert_eq!(published[1]["error"], "No management key configured.");
        // An unchanged refresh, or a redemption that changes nothing, publishes nothing new.
        service.refresh().await;
        let result = service.consume_reset_credit("hub", "pooled.json", "credit").await.unwrap();
        assert_eq!(result, json!({"outcome": "reset"}));
        assert!(tokio::time::timeout(Duration::from_millis(50), changes.next()).await.is_err());
        assert!(service.consume_reset_credit("off", "pooled.json", "credit").await.is_err());
    }

    #[tokio::test]
    async fn config_events_only_for_subscribers_that_ask() {
        let events = UsageLimitSourcesEvents(sources(json!({})));
        assert!(events.subscribe(&ConfigOptions::default()).is_none());
        let mut stream = events
            .subscribe(&ConfigOptions {
                usage_limit_sources: true,
                ..ConfigOptions::default()
            })
            .unwrap();
        assert_eq!(
            stream.next().await.unwrap(),
            json!({"version": 1, "type": "usageLimitSourcesUpdated", "payload": {"sources": []}})
        );
    }
}
