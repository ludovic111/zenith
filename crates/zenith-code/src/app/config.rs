//! The parts of `ServerConfig` that zc-settings does not own (`ws.ts` `loadServerConfig`),
//! plugged into its [`ServerConfigService`](zc_settings::ServerConfigService) as snapshot
//! contributors and live event sources:
//!
//! | Field / event | Source |
//! |---|---|
//! | `environment` | the descriptor ([`super::environment::ServerEnvironment`]) |
//! | `auth` | zc-auth's `ServerAuthDescriptor` |
//! | `providers`, `providerStatuses` | zc-providers' registry (200 ms debounce, repeats of the snapshot dropped) |
//! | `availableEditors`, `shellRevealInFileManager[Kind]` | an [`EditorDiscovery`] (WP-24; none by default) |
//! | `remoteOpenTargets` | sshd on loopback → tailnet name and `<host>.local` |
//!
//! Discovery that shells out is bounded by 5 s (`CONFIG_DISCOVERY_TIMEOUT`) and degrades to
//! nothing, so a slow probe never stalls `server.getConfig`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use serde_json::{json, Map, Value};
use zc_providers::ProviderRegistry;
use zc_settings::config::{ConfigError, ConfigEventSource, ConfigOptions, SnapshotContributor};

/// `CONFIG_DISCOVERY_TIMEOUT`.
pub const CONFIG_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// `PROVIDER_STATUS_DEBOUNCE_MS`.
pub const PROVIDER_STATUS_DEBOUNCE: Duration = Duration::from_millis(200);

/// `ExternalLauncher.resolveAvailableEditors` / `resolveFileManagerRevealKind` (WP-24).
#[async_trait]
pub trait EditorDiscovery: Send + Sync {
    /// Encoded `EditorId`s.
    async fn available_editors(&self) -> Vec<Value>;
    /// `finder | file-explorer | files`, when a file manager is among the editors.
    async fn file_manager_reveal_kind(&self) -> Option<String>;
}

/// No editors (until WP-24 plugs its launcher in).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoEditors;

#[async_trait]
impl EditorDiscovery for NoEditors {
    async fn available_editors(&self) -> Vec<Value> {
        Vec::new()
    }
    async fn file_manager_reveal_kind(&self) -> Option<String> {
        None
    }
}

async fn bounded<T>(future: impl std::future::Future<Output = T>, fallback: T) -> T {
    tokio::time::timeout(CONFIG_DISCOVERY_TIMEOUT, future).await.unwrap_or(fallback)
}

/// `environment`, `auth`, `availableEditors`, `remoteOpenTargets` and the file-manager reveal.
pub struct EnvironmentContributor {
    pub environment: Value,
    pub auth: Value,
    pub editors: Arc<dyn EditorDiscovery>,
    pub remote_open_targets: bool,
}

#[async_trait]
impl SnapshotContributor for EnvironmentContributor {
    async fn contribute(&self, config: &mut Map<String, Value>, _options: &ConfigOptions) -> Result<(), ConfigError> {
        config.insert("environment".into(), self.environment.clone());
        config.insert("auth".into(), self.auth.clone());
        let editors = bounded(self.editors.available_editors(), Vec::new()).await;
        let reveal_kind = if editors.iter().any(|editor| editor == "file-manager") {
            bounded(self.editors.file_manager_reveal_kind(), None).await
        } else {
            None
        };
        config.insert("availableEditors".into(), Value::Array(editors));
        let targets = if self.remote_open_targets {
            bounded(super::environment::resolve_remote_open_targets(), Vec::new()).await
        } else {
            Vec::new()
        };
        config.insert("remoteOpenTargets".into(), Value::Array(targets));
        if let Some(kind) = reveal_kind {
            config.insert("shellRevealInFileManager".into(), json!(true));
            config.insert("shellRevealInFileManagerKind".into(), json!(kind));
        }
        Ok(())
    }
}

fn encoded_providers(registry: &ProviderRegistry) -> Value {
    serde_json::to_value(registry.get_providers()).unwrap_or_else(|_| json!([]))
}

/// `providers`: the registry's current statuses.
pub struct ProvidersContributor(pub ProviderRegistry);

#[async_trait]
impl SnapshotContributor for ProvidersContributor {
    async fn contribute(&self, config: &mut Map<String, Value>, _options: &ConfigOptions) -> Result<(), ConfigError> {
        config.insert("providers".into(), encoded_providers(&self.0));
        Ok(())
    }
}

/// `providerStatuses`: the full provider list after each change, debounced, without the
/// first value when it repeats the snapshot the client already holds.
pub struct ProviderStatusSource(pub ProviderRegistry);

impl ConfigEventSource for ProviderStatusSource {
    fn subscribe(&self, _options: &ConfigOptions) -> Option<BoxStream<'static, Value>> {
        let changes = self
            .0
            .subscribe_changes()
            .map(|providers| serde_json::to_value(providers).unwrap_or_else(|_| json!([])));
        Some(changes.boxed())
    }

    fn with_snapshot(&self, events: BoxStream<'static, Value>, snapshot: &Value) -> BoxStream<'static, Value> {
        let previous = snapshot.get("providers").cloned().unwrap_or_else(|| json!([]));
        let distinct = events
            .scan(previous, |previous, providers| {
                let changed = *previous != providers;
                *previous = providers.clone();
                futures::future::ready(Some(changed.then_some(providers)))
            })
            .filter_map(futures::future::ready);
        debounce(distinct.boxed(), PROVIDER_STATUS_DEBOUNCE)
            .map(|providers| json!({ "version": 1, "type": "providerStatuses", "payload": { "providers": providers } }))
            .boxed()
    }
}

/// `Stream.debounce`: emits the latest value once `quiet` passed without a newer one.
pub fn debounce<T: Send + 'static>(source: BoxStream<'static, T>, quiet: Duration) -> BoxStream<'static, T> {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut source = source;
        let mut pending: Option<T> = None;
        loop {
            let waiting = pending.is_some();
            let next = tokio::select! {
                // The subscriber went away: stop reading the source.
                _ = sender.closed() => return,
                next = source.next() => Some(next),
                _ = tokio::time::sleep(quiet), if waiting => None,
            };
            match next {
                // Quiet period over: emit the latest value.
                None => {
                    if let Some(value) = pending.take() {
                        if sender.send(value).is_err() {
                            return;
                        }
                    }
                }
                Some(Some(value)) => pending = Some(value),
                Some(None) => {
                    if let Some(value) = pending.take() {
                        let _ = sender.send(value);
                    }
                    return;
                }
            }
        }
    });
    tokio_stream::wrappers::UnboundedReceiverStream::new(receiver).boxed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn debounce_keeps_the_latest_value() {
        let (sender, receiver) = futures::channel::mpsc::unbounded::<u32>();
        let mut out = debounce(receiver.boxed(), Duration::from_millis(200));
        sender.unbounded_send(1).unwrap();
        sender.unbounded_send(2).unwrap();
        assert_eq!(out.next().await, Some(2));
        sender.unbounded_send(3).unwrap();
        drop(sender);
        assert_eq!(out.next().await, Some(3));
        assert_eq!(out.next().await, None);
    }
}
