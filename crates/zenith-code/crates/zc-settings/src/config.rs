//! `server.getConfig` and `subscribeServerConfig` (`ws.ts` `loadServerConfig` and the
//! `subscribeServerConfig` handler), as a composable builder.
//!
//! This crate owns the settings, keybindings and themes parts. Everything else in
//! `ServerConfig` (environment descriptor, auth descriptor, providers, editors, remote open
//! targets, file-manager reveal) and the other live events (`providerStatuses`,
//! `usageLimitSourcesUpdated`) belong to other packages; they plug in as:
//!
//! - [`SnapshotContributor`]s: each fills fields of the snapshot object (`environment`,
//!   `auth`, `providers`, `availableEditors`, …). They run in registration order on every
//!   `getConfig` / subscription.
//! - [`ConfigEventSource`]s: each may add a stream of encoded `ServerConfigStreamEvent`s to a
//!   subscription, depending on the payload flags. Sources subscribe eagerly, before the
//!   snapshot is read, so nothing between the two is lost.
//!
//! The snapshot is assembled as encoded JSON in `ServerConfig` declaration order.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use serde_json::{json, Map, Value};
use zc_contracts::{KeybindingsConfigError, ServerSettingsError};

use crate::keybindings::{KeybindingsConfigState, KeybindingsService};
use crate::settings::{redact_server_settings_for_client, ServerSettingsService};
use crate::themes::EnvironmentThemeService;

/// `ServerConfig` keys in declaration order (`packages/contracts/src/server.ts`).
pub const SERVER_CONFIG_KEYS: &[&str] = &[
    "environment",
    "auth",
    "cwd",
    "keybindingsConfigPath",
    "keybindings",
    "issues",
    "providers",
    "availableEditors",
    "remoteOpenTargets",
    "observability",
    "settings",
    "shellResumeCompletionMarker",
    "shellRevealInFileManager",
    "shellRevealInFileManagerKind",
    "threadResumeCompletionMarker",
    "threadSnapshotPagination",
    "reasoningMessages",
    "environmentThemes",
    "usageLimitSources",
];

/// The `subscribeServerConfig` payload flags (all default to false; `getConfig` uses none).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConfigOptions {
    pub environment_themes: bool,
    pub usage_limit_sources: bool,
    pub usage_limits_command: bool,
}

impl ConfigOptions {
    /// From the `subscribeServerConfig` payload (`{environmentThemes?, usageLimitSources?,
    /// usageLimitsCommand?}`; anything but `true` is false).
    pub fn from_payload(payload: &Value) -> Self {
        let flag = |key: &str| payload.get(key) == Some(&Value::Bool(true));
        Self {
            environment_themes: flag("environmentThemes"),
            usage_limit_sources: flag("usageLimitSources"),
            usage_limits_command: flag("usageLimitsCommand"),
        }
    }
}

/// Why the config could not be produced: the RPC's error union, or a defect.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigError {
    Keybindings(KeybindingsConfigError),
    Settings(ServerSettingsError),
    /// An unexpected failure of a contributor (becomes `Die`).
    Defect(String),
}

impl From<KeybindingsConfigError> for ConfigError {
    fn from(error: KeybindingsConfigError) -> Self {
        Self::Keybindings(error)
    }
}

impl From<ServerSettingsError> for ConfigError {
    fn from(error: ServerSettingsError) -> Self {
        Self::Settings(error)
    }
}

/// Fills fields of the `ServerConfig` snapshot owned by another package.
#[async_trait]
pub trait SnapshotContributor: Send + Sync {
    async fn contribute(&self, config: &mut Map<String, Value>, options: &ConfigOptions) -> Result<(), ConfigError>;
}

/// A live `subscribeServerConfig` event source owned by another package.
pub trait ConfigEventSource: Send + Sync {
    /// Subscribe now (eagerly) and return the encoded events to merge into the subscription,
    /// or `None` when `options` do not ask for them. `snapshot` is not read yet at that point;
    /// sources that compare against the snapshot (provider statuses) get it through
    /// [`ConfigEventSource::with_snapshot`].
    fn subscribe(&self, options: &ConfigOptions) -> Option<BoxStream<'static, Value>>;

    /// Adjust the subscribed stream once the snapshot is known (default: unchanged), e.g. to
    /// drop a first event that repeats the snapshot.
    fn with_snapshot(&self, events: BoxStream<'static, Value>, _snapshot: &Value) -> BoxStream<'static, Value> {
        events
    }
}

/// The fixed parts of the snapshot that come from the server configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerConfigParts {
    /// `config.cwd`.
    pub cwd: String,
    /// The encoded `ServerObservability` ([`observability`]).
    pub observability: Value,
}

/// `ServerObservability` as `loadServerConfig` builds it.
pub fn observability(logs_directory_path: &str, otlp_traces_url: Option<&str>, otlp_metrics_url: Option<&str>, otlp_logs_url: Option<&str>) -> Value {
    let mut value = Map::new();
    value.insert("logsDirectoryPath".into(), json!(logs_directory_path));
    value.insert("localTracingEnabled".into(), json!(true));
    if let Some(url) = otlp_traces_url {
        value.insert("otlpTracesUrl".into(), json!(url));
    }
    value.insert("otlpTracesEnabled".into(), json!(otlp_traces_url.is_some()));
    if let Some(url) = otlp_metrics_url {
        value.insert("otlpMetricsUrl".into(), json!(url));
    }
    value.insert("otlpMetricsEnabled".into(), json!(otlp_metrics_url.is_some()));
    if let Some(url) = otlp_logs_url {
        value.insert("otlpLogsUrl".into(), json!(url));
    }
    value.insert("otlpLogsEnabled".into(), json!(otlp_logs_url.is_some()));
    Value::Object(value)
}

/// Encoded `{keybindings, issues}`.
pub fn keybindings_payload(state: &KeybindingsConfigState) -> Value {
    json!({
        "keybindings": serde_json::to_value(&state.keybindings).unwrap_or(Value::Array(Vec::new())),
        "issues": serde_json::to_value(&state.issues).unwrap_or(Value::Array(Vec::new())),
    })
}

fn event(kind: &str, payload: Value) -> Value {
    json!({"version": 1, "type": kind, "payload": payload})
}

/// Builds `ServerConfig` snapshots and `subscribeServerConfig` streams.
#[derive(Clone)]
pub struct ServerConfigService {
    settings: ServerSettingsService,
    keybindings: KeybindingsService,
    themes: Option<EnvironmentThemeService>,
    parts: ServerConfigParts,
    contributors: Vec<Arc<dyn SnapshotContributor>>,
    sources: Vec<Arc<dyn ConfigEventSource>>,
}

impl ServerConfigService {
    pub fn new(settings: ServerSettingsService, keybindings: KeybindingsService, parts: ServerConfigParts) -> Self {
        Self {
            settings,
            keybindings,
            themes: None,
            parts,
            contributors: Vec::new(),
            sources: Vec::new(),
        }
    }

    /// Publish `environmentThemesUpdated` to subscribers that ask for it.
    pub fn with_themes(mut self, themes: EnvironmentThemeService) -> Self {
        self.themes = Some(themes);
        self
    }

    /// Add a snapshot contributor (environment, auth, providers, editors, …).
    pub fn with_contributor(mut self, contributor: Arc<dyn SnapshotContributor>) -> Self {
        self.contributors.push(contributor);
        self
    }

    /// Add a live event source (provider statuses, usage limit sources, …).
    pub fn with_event_source(mut self, source: Arc<dyn ConfigEventSource>) -> Self {
        self.sources.push(source);
        self
    }

    pub fn settings(&self) -> &ServerSettingsService {
        &self.settings
    }

    pub fn keybindings(&self) -> &KeybindingsService {
        &self.keybindings
    }

    /// `loadServerConfig`: the encoded `ServerConfig`.
    pub async fn load(&self, options: &ConfigOptions) -> Result<Value, ConfigError> {
        let keybindings = self.keybindings.load_config_state().await?;
        let settings = redact_server_settings_for_client(self.settings.get_settings_value().await?);
        let mut config = Map::new();
        config.insert("environment".into(), Value::Null);
        config.insert("auth".into(), Value::Null);
        config.insert("cwd".into(), json!(self.parts.cwd));
        config.insert("keybindingsConfigPath".into(), json!(self.keybindings.config_path().to_string_lossy()));
        let payload = keybindings_payload(&keybindings);
        config.insert("keybindings".into(), payload["keybindings"].clone());
        config.insert("issues".into(), payload["issues"].clone());
        config.insert("providers".into(), json!([]));
        config.insert("availableEditors".into(), json!([]));
        config.insert("observability".into(), self.parts.observability.clone());
        config.insert("settings".into(), settings);
        config.insert("shellResumeCompletionMarker".into(), json!(true));
        config.insert("threadResumeCompletionMarker".into(), json!(true));
        config.insert("threadSnapshotPagination".into(), json!(true));
        config.insert("reasoningMessages".into(), json!(true));
        for contributor in &self.contributors {
            contributor.contribute(&mut config, options).await?;
        }
        Ok(Value::Object(order_config_keys(config)))
    }

    /// `subscribeServerConfig`: the snapshot, then `keybindingsUpdated`, `settingsUpdated`,
    /// `environmentThemesUpdated` (when asked for) and the sources' events. Never completes.
    pub async fn subscribe(&self, options: ConfigOptions) -> Result<BoxStream<'static, Value>, ConfigError> {
        let keybindings_updates = self
            .keybindings
            .subscribe_changes()
            .map(|state| event("keybindingsUpdated", keybindings_payload(&state)))
            .boxed();
        let settings_updates = self
            .settings
            .subscribe_changes_value()
            .map(|settings| event("settingsUpdated", json!({"settings": redact_server_settings_for_client(settings)})))
            .boxed();
        let theme_updates = match (&self.themes, options.environment_themes) {
            (Some(themes), true) => Some(
                themes
                    .stream_changes()
                    .await
                    .map(|themes| event("environmentThemesUpdated", json!({"themes": themes})))
                    .boxed(),
            ),
            _ => None,
        };
        let subscribed: Vec<(Arc<dyn ConfigEventSource>, BoxStream<'static, Value>)> = self
            .sources
            .iter()
            .filter_map(|source| source.subscribe(&options).map(|events| (source.clone(), events)))
            .collect();
        let config = self.load(&options).await?;
        let mut live: Vec<BoxStream<'static, Value>> = vec![keybindings_updates, settings_updates];
        live.extend(theme_updates);
        for (source, events) in subscribed {
            live.push(source.with_snapshot(events, &config));
        }
        let snapshot = json!({"version": 1, "type": "snapshot", "config": config});
        Ok(stream::once(async move { snapshot })
            .chain(stream::select_all(live))
            .chain(stream::pending())
            .boxed())
    }
}

/// `ServerConfig` declaration order; unknown keys keep their order after the known ones.
fn order_config_keys(mut config: Map<String, Value>) -> Map<String, Value> {
    let mut ordered = Map::new();
    for key in SERVER_CONFIG_KEYS {
        if let Some(value) = config.shift_remove(*key) {
            ordered.insert((*key).to_owned(), value);
        }
    }
    ordered.append(&mut config);
    ordered
}
