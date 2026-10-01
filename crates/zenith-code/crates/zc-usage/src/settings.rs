//! The settings the usage package reads: the decoded `ServerSettings` (secrets materialized)
//! and its later changes.

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::Value;

/// `ServerSettingsService` as the usage code sees it.
#[async_trait]
pub trait UsageSettings: Send + Sync {
    /// `getSettings`: the decoded settings, secrets materialized.
    async fn get(&self) -> Result<Value, String>;
    /// `streamChanges`: every later change.
    fn changes(&self) -> BoxStream<'static, Value>;
}

#[async_trait]
impl UsageSettings for zc_settings::ServerSettingsService {
    async fn get(&self) -> Result<Value, String> {
        self.get_settings_value().await.map_err(|error| format!("{error:?}"))
    }

    fn changes(&self) -> BoxStream<'static, Value> {
        self.subscribe_changes_value()
    }
}
