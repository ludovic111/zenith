//! The RPC handlers (`ws.ts`): `server.getSettings`, `server.updateSettings`,
//! `server.upsertKeybinding`, `server.removeKeybinding`, `server.getConfig`,
//! `subscribeServerConfig`.
//!
//! They are registered over raw JSON so the encoded values keep the order TS gives them
//! (record keys in insertion order); payloads are still validated against the generated types
//! first, and a payload that does not decode is a per-request `Die`, like the TS server.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use zc_contracts::{ServerRemoveKeybindingInput, ServerUpdateSettingsPayload, ServerUpsertKeybindingInput, SubscribeServerConfigPayload};
use zc_rpc::{RpcError, RpcRouterBuilder};

use crate::config::{ConfigError, ConfigOptions, ServerConfigService};
use crate::settings::redact_server_settings_for_client;

/// Rewrites an `updateSettings` patch before it is applied (`ws.ts` resolves `deviceHosts`
/// through the SSH device host service; that belongs to the device package).
#[async_trait]
pub trait SettingsPatchHook: Send + Sync {
    async fn rewrite(&self, patch: Value) -> Result<Value, RpcError>;
}

/// What the handlers need.
#[derive(Clone)]
pub struct SettingsRpc {
    pub config: ServerConfigService,
    pub patch_hook: Option<Arc<dyn SettingsPatchHook>>,
}

fn decode<T: serde::de::DeserializeOwned>(payload: &Value) -> Result<T, RpcError> {
    serde_json::from_value(payload.clone()).map_err(|error| RpcError::die_text(error.to_string()))
}

fn config_error(error: ConfigError) -> RpcError {
    match error {
        ConfigError::Keybindings(error) => RpcError::fail(error),
        ConfigError::Settings(error) => RpcError::fail(error),
        ConfigError::Defect(message) => RpcError::die(message),
    }
}

impl SettingsRpc {
    pub fn new(config: ServerConfigService) -> Self {
        Self { config, patch_hook: None }
    }

    pub fn with_patch_hook(mut self, hook: Arc<dyn SettingsPatchHook>) -> Self {
        self.patch_hook = Some(hook);
        self
    }

    /// `server.getSettings`.
    pub async fn get_settings(&self) -> Result<Value, RpcError> {
        let settings = self.config.settings().get_settings_value().await.map_err(RpcError::fail)?;
        Ok(redact_server_settings_for_client(settings))
    }

    /// `server.updateSettings({patch})`.
    pub async fn update_settings(&self, payload: Value) -> Result<Value, RpcError> {
        decode::<ServerUpdateSettingsPayload>(&payload)?;
        let mut patch = payload.get("patch").cloned().unwrap_or_else(|| json!({}));
        // The schema checks the generated types skip (trimmed non-empty strings, …) are payload
        // decode failures in TS: a per-request `Die`.
        crate::settings::schema::decode_patch(&patch).map_err(|issue| RpcError::die_text(issue.message))?;
        if let Some(hook) = &self.patch_hook {
            if patch.get("deviceHosts").is_some() {
                patch = hook.rewrite(patch).await?;
            }
        }
        let settings = self.config.settings().update_settings_value(&patch).await.map_err(RpcError::fail)?;
        Ok(redact_server_settings_for_client(settings))
    }

    /// `server.upsertKeybinding`.
    pub async fn upsert_keybinding(&self, payload: Value) -> Result<Value, RpcError> {
        let input: ServerUpsertKeybindingInput = decode(&payload)?;
        let keybindings = self.config.keybindings().upsert_keybinding_rule(&input).await.map_err(RpcError::fail)?;
        Ok(json!({"keybindings": keybindings, "issues": []}))
    }

    /// `server.removeKeybinding`.
    pub async fn remove_keybinding(&self, payload: Value) -> Result<Value, RpcError> {
        let input: ServerRemoveKeybindingInput = decode(&payload)?;
        let keybindings = self.config.keybindings().remove_keybinding_rule(&input).await.map_err(RpcError::fail)?;
        Ok(json!({"keybindings": keybindings, "issues": []}))
    }

    /// `server.getConfig`.
    pub async fn get_config(&self) -> Result<Value, RpcError> {
        self.config.load(&ConfigOptions::default()).await.map_err(config_error)
    }

    /// `subscribeServerConfig`.
    pub async fn subscribe_server_config(&self, payload: Value) -> Result<futures::stream::BoxStream<'static, Result<Value, RpcError>>, RpcError> {
        decode::<SubscribeServerConfigPayload>(&payload)?;
        let events = self.config.subscribe(ConfigOptions::from_payload(&payload)).await.map_err(config_error)?;
        Ok(events.map(Ok).boxed())
    }

    /// Register the six methods (scopes come from the router's scope table).
    pub fn register(self, builder: RpcRouterBuilder) -> RpcRouterBuilder {
        let rpc = Arc::new(self);
        let get_settings = rpc.clone();
        let update_settings = rpc.clone();
        let upsert = rpc.clone();
        let remove = rpc.clone();
        let get_config = rpc.clone();
        let subscribe = rpc;
        builder
            .unary("server.getSettings", move |_ctx, _payload| {
                let rpc = get_settings.clone();
                async move { rpc.get_settings().await }
            })
            .unary("server.updateSettings", move |_ctx, payload| {
                let rpc = update_settings.clone();
                async move { rpc.update_settings(payload).await }
            })
            .unary("server.upsertKeybinding", move |_ctx, payload| {
                let rpc = upsert.clone();
                async move { rpc.upsert_keybinding(payload).await }
            })
            .unary("server.removeKeybinding", move |_ctx, payload| {
                let rpc = remove.clone();
                async move { rpc.remove_keybinding(payload).await }
            })
            .unary("server.getConfig", move |_ctx, _payload| {
                let rpc = get_config.clone();
                async move { rpc.get_config().await }
            })
            .stream("subscribeServerConfig", move |_ctx, payload| {
                let rpc = subscribe.clone();
                async move { rpc.subscribe_server_config(payload).await }
            })
    }
}
