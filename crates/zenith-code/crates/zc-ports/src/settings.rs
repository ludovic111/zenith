//! The settings port (`apps/server/src/serverSettings.ts` `ServerSettingsService`).
//!
//! Implemented by zc-settings; consumed by nearly everything (providers, reactors, VCS pollers,
//! background policy, usage, terminal env). Lifecycle (`start`, `ready`) stays on the concrete
//! type, which the server wiring owns.

use async_trait::async_trait;

use crate::contracts::{ServerSettings, ServerSettingsError, ServerSettingsPatch};
use crate::EventStream;

#[async_trait]
pub trait SettingsService: Send + Sync {
    /// `getSettings`: the current settings, secrets materialized (provider environment values,
    /// usage-hub keys and Bitbucket tokens are the real values: consumers need them; RPCs
    /// redact before answering, like `ws.ts`).
    async fn get_settings(&self) -> Result<ServerSettings, ServerSettingsError>;

    /// `updateSettings(patch)`: apply, persist sparsely and atomically, return the new settings.
    async fn update_settings(&self, patch: ServerSettingsPatch) -> Result<ServerSettings, ServerSettingsError>;

    /// `subscribeChanges`: every settings value published after this call returns (file edits
    /// are picked up by a 100 ms debounced watch). Call it before [`Self::get_settings`] when no
    /// change between the read and the stream may be lost.
    fn subscribe_changes(&self) -> EventStream<ServerSettings>;
}
