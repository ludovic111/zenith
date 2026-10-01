//! `ServerSettingsService` (`apps/server/src/serverSettings.ts`, `layer`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Map, Value};
use tokio::sync::watch;
use zc_contracts::{ServerSettings, ServerSettingsError, ServerSettingsOperation, ServerSettingsPatch};
use zc_core::{Defect, PubSub};

use super::logic::{
    self, LegacyProjectSettingsRow, PersistedProviderFlags, ProviderHistoryEntry, SecretChange, SecretChangeKind, BITBUCKET_SECRET_FIELDS, SECRET_REDACTED,
};
use super::schema::{decode_patch, decode_settings, default_settings, to_typed_settings};
use crate::errors::{defect, settings_error, settings_error_at, with_secret_context};
use crate::js::stringify_pretty;
use crate::watch::{file_filter, watch_directory, DirWatch, WATCH_DEBOUNCE};

/// The secret store as the settings service uses it (`ServerSecretStore` get/set/remove).
#[async_trait]
pub trait SecretBackend: Send + Sync {
    async fn get(&self, name: &str) -> Result<Option<Vec<u8>>, Defect>;
    async fn set(&self, name: &str, value: &[u8]) -> Result<(), Defect>;
    async fn remove(&self, name: &str) -> Result<(), Defect>;
}

fn secret_defect(error: &zc_core::secrets::SecretStoreError) -> Defect {
    Defect::error(error.tag(), error.to_string())
}

#[async_trait]
impl SecretBackend for zc_core::ServerSecretStore {
    async fn get(&self, name: &str) -> Result<Option<Vec<u8>>, Defect> {
        zc_core::ServerSecretStore::get(self, name).await.map_err(|error| secret_defect(&error))
    }

    async fn set(&self, name: &str, value: &[u8]) -> Result<(), Defect> {
        zc_core::ServerSecretStore::set(self, name, value).await.map_err(|error| secret_defect(&error))
    }

    async fn remove(&self, name: &str) -> Result<(), Defect> {
        zc_core::ServerSecretStore::remove(self, name).await.map_err(|error| secret_defect(&error))
    }
}

/// The SQLite reads the settings load needs (provider history and the legacy project columns).
#[async_trait]
pub trait SettingsDatabase: Send + Sync {
    /// Distinct `(provider_name, provider_instance_id)` of cursor/grok/opencode sessions, from
    /// `projection_thread_sessions` and `provider_session_runtime`.
    async fn provider_history(&self) -> Result<Vec<ProviderHistoryEntry>, Defect>;
    /// The settings columns of every live project.
    async fn legacy_project_settings(&self) -> Result<Vec<LegacyProjectSettingsRow>, Defect>;
}

const PROVIDER_HISTORY_SQL: &str = r#"
      SELECT DISTINCT
        provider_name AS "providerName",
        provider_instance_id AS "providerInstanceId"
      FROM projection_thread_sessions
      WHERE provider_name IN ('cursor', 'grok', 'opencode')
      UNION
      SELECT DISTINCT
        provider_name AS "providerName",
        provider_instance_id AS "providerInstanceId"
      FROM provider_session_runtime
      WHERE provider_name IN ('cursor', 'grok', 'opencode')
    "#;

const LEGACY_PROJECT_SETTINGS_SQL: &str = r#"
          SELECT
            project_id AS "projectId",
            default_model_selection_json AS "defaultModelSelection",
            default_thread_env_mode AS "defaultThreadEnvMode",
            auto_pull AS "autoPull",
            scripts_json AS "scripts"
          FROM projection_projects
          WHERE deleted_at IS NULL
        "#;

#[async_trait]
impl SettingsDatabase for zc_db::Db {
    async fn provider_history(&self) -> Result<Vec<ProviderHistoryEntry>, Defect> {
        self.read(|conn| {
            let run = || -> rusqlite::Result<Vec<ProviderHistoryEntry>> {
                let mut statement = conn.prepare(PROVIDER_HISTORY_SQL)?;
                let rows = statement.query_map([], |row| {
                    Ok(ProviderHistoryEntry {
                        provider_name: row.get(0)?,
                        provider_instance_id: row.get(1)?,
                    })
                })?;
                rows.collect()
            };
            run().map_err(|error| zc_db::DbError::sql("ServerSettings.readProviderHistory", error))
        })
        .await
        .map_err(|error| defect(error.to_string()))
    }

    async fn legacy_project_settings(&self) -> Result<Vec<LegacyProjectSettingsRow>, Defect> {
        self.read(|conn| {
            let run = || -> rusqlite::Result<Vec<LegacyProjectSettingsRow>> {
                let mut statement = conn.prepare(LEGACY_PROJECT_SETTINGS_SQL)?;
                let rows = statement.query_map([], |row| {
                    Ok(LegacyProjectSettingsRow {
                        project_id: row.get(0)?,
                        default_model_selection: row.get(1)?,
                        default_thread_env_mode: row.get(2)?,
                        auto_pull: row.get(3)?,
                        scripts: row.get(4)?,
                    })
                })?;
                rows.collect()
            };
            run().map_err(|error| zc_db::DbError::sql("ServerSettings.readProjectSettings", error))
        })
        .await
        .map_err(|error| defect(error.to_string()))
    }
}

type LoadResult = Result<Value, ServerSettingsError>;

struct Inner {
    settings_path: PathBuf,
    settings_path_text: String,
    secrets: Arc<dyn SecretBackend>,
    database: Arc<dyn SettingsDatabase>,
    /// `writeSemaphore`: updates and watch reloads, one at a time.
    write_lock: tokio::sync::Mutex<()>,
    /// The Effect `Cache` (capacity 1, no TTL: a failed load stays cached until invalidated).
    /// Holds the persisted form: secrets are markers.
    cache: tokio::sync::Mutex<Option<LoadResult>>,
    /// Every settings value published (persisted form).
    changes: PubSub<Value>,
    started: tokio::sync::OnceCell<Result<(), ServerSettingsError>>,
    ready: watch::Sender<Option<Result<(), ServerSettingsError>>>,
    watcher: Mutex<Option<DirWatch>>,
}

/// The settings service. Cheap to clone.
#[derive(Clone)]
pub struct ServerSettingsService {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for ServerSettingsService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerSettingsService")
            .field("settings_path", &self.inner.settings_path)
            .finish_non_exhaustive()
    }
}

impl ServerSettingsService {
    /// `layer`: the service over `settings_path`. Nothing is read until the first use.
    pub fn new(settings_path: impl Into<PathBuf>, secrets: Arc<dyn SecretBackend>, database: Arc<dyn SettingsDatabase>) -> Self {
        let settings_path = settings_path.into();
        let (ready, _) = watch::channel(None);
        Self {
            inner: Arc::new(Inner {
                settings_path_text: settings_path.to_string_lossy().into_owned(),
                settings_path,
                secrets,
                database,
                write_lock: tokio::sync::Mutex::new(()),
                cache: tokio::sync::Mutex::new(None),
                changes: PubSub::new(),
                started: tokio::sync::OnceCell::new(),
                ready,
                watcher: Mutex::new(None),
            }),
        }
    }

    /// The `settings.json` path.
    pub fn settings_path(&self) -> &Path {
        &self.inner.settings_path
    }

    fn error(&self, operation: ServerSettingsOperation, cause: Defect) -> ServerSettingsError {
        settings_error(&self.inner.settings_path_text, operation, cause)
    }

    /// `start`: create the directory, attach the watcher, load. Safe to call repeatedly; later
    /// calls await the first one's outcome.
    pub async fn start(&self) -> Result<(), ServerSettingsError> {
        let outcome = self
            .inner
            .started
            .get_or_init(|| async {
                let outcome = async {
                    self.start_watcher().await?;
                    self.invalidate().await;
                    self.cached().await.map(|_| ())
                }
                .await;
                self.inner.ready.send_replace(Some(outcome.clone()));
                outcome
            })
            .await;
        outcome.clone()
    }

    /// `ready`: wait until [`Self::start`] has finished, with its outcome.
    pub async fn ready(&self) -> Result<(), ServerSettingsError> {
        let mut receiver = self.inner.ready.subscribe();
        loop {
            if let Some(outcome) = receiver.borrow_and_update().clone() {
                return outcome;
            }
            if receiver.changed().await.is_err() {
                return Ok(());
            }
        }
    }

    async fn start_watcher(&self) -> Result<(), ServerSettingsError> {
        let path = &self.inner.settings_path;
        let directory = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        tokio::fs::create_dir_all(&directory)
            .await
            .map_err(|error| self.error(ServerSettingsOperation::PrepareDirectory, Defect::from(&error)))?;
        let service = self.clone();
        let handler: crate::watch::ChangeHandler = Arc::new(move || {
            let service = service.clone();
            Box::pin(async move {
                if let Err(error) = service.revalidate_and_emit().await {
                    tracing::warn!(error = %crate::errors::settings_error_message(&error), "settings reload failed");
                }
            })
        });
        match watch_directory(&directory, file_filter(path), WATCH_DEBOUNCE, handler) {
            Ok(watch) => {
                *self.inner.watcher.lock().unwrap_or_else(|p| p.into_inner()) = Some(watch);
            }
            // `Effect.ignoreCause({ log: true })` around the watch.
            Err(error) => tracing::warn!(%error, "could not watch the settings directory"),
        }
        Ok(())
    }

    /// `revalidateAndEmit`: reload from disk and publish.
    pub async fn revalidate_and_emit(&self) -> Result<(), ServerSettingsError> {
        let _permit = self.inner.write_lock.lock().await;
        self.invalidate().await;
        let settings = self.cached().await?;
        self.inner.changes.publish(settings);
        Ok(())
    }

    async fn invalidate(&self) {
        *self.inner.cache.lock().await = None;
    }

    /// `Cache.get`: the persisted-form settings, loading them on a miss.
    async fn cached(&self) -> LoadResult {
        let mut cache = self.inner.cache.lock().await;
        if let Some(result) = cache.as_ref() {
            return result.clone();
        }
        let result = self.load_from_disk().await;
        *cache = Some(result.clone());
        result
    }

    async fn set_cache(&self, settings: Value) {
        *self.inner.cache.lock().await = Some(Ok(settings));
    }

    /// `loadSettingsFromDisk`.
    async fn load_from_disk(&self) -> LoadResult {
        let path = &self.inner.settings_path;
        let mut settings = default_settings();
        let mut persisted = PersistedProviderFlags::default();
        let mut trusted = true;
        let exists = tokio::fs::try_exists(path)
            .await
            .map_err(|error| self.error(ServerSettingsOperation::CheckExists, Defect::from(&error)))?;
        if exists {
            let raw = tokio::fs::read_to_string(path)
                .await
                .map_err(|error| self.error(ServerSettingsOperation::ReadFile, Defect::from(&error)))?;
            let parsed = zc_core::parse_lenient_json(&raw);
            let decoded = parsed
                .as_ref()
                .map_err(|error| error.to_string())
                .and_then(|value| decode_settings(value).map_err(|issue| issue.message));
            let flags = parsed.as_ref().ok().and_then(PersistedProviderFlags::decode);
            if let Some(flags) = &flags {
                persisted = flags.clone();
            }
            match (decoded, flags) {
                (Ok(value), Some(_)) => settings = value,
                (decoded, _) => {
                    trusted = false;
                    tracing::warn!(
                        path = %self.inner.settings_path_text,
                        issues = %decoded.err().unwrap_or_else(|| "invalid providers".into()),
                        "failed to parse settings.json, using defaults"
                    );
                }
            }
        }
        let history = self
            .inner
            .database
            .provider_history()
            .await
            .map_err(|cause| self.error(ServerSettingsOperation::ReadProviderHistory, cause))?;
        let rows = if settings["projectSettingsFolded"] == Value::Bool(true) || !trusted {
            Vec::new()
        } else {
            self.inner
                .database
                .legacy_project_settings()
                .await
                .map_err(|cause| self.error(ServerSettingsOperation::ReadProjectSettings, cause))?
        };
        let mut loaded = logic::restore_used_providers(settings, &persisted, &history);
        logic::fold_provider_instance_enabled_flags(&mut loaded);
        let mut migrated = loaded.clone();
        let mut changed = false;
        if trusted {
            let folded = logic::fold_legacy_project_settings(loaded.clone(), &rows);
            changed |= folded != loaded;
            let (moved, any_moved) = self.move_inline_bitbucket_tokens(folded).await;
            changed |= any_moved;
            migrated = moved;
        }
        let migrated = canonical(migrated);
        if changed {
            self.write_settings(&migrated).await?;
        }
        Ok(migrated)
    }

    /// `moveInlineBitbucketTokens`: plaintext tokens hand-edited into the file move to the secret
    /// store (a failing store leaves them in the file, retried on the next load).
    async fn move_inline_bitbucket_tokens(&self, mut settings: Value) -> (Value, bool) {
        let mut moved = false;
        for (field, secret_name) in BITBUCKET_SECRET_FIELDS {
            let value = settings["bitbucket"][field].as_str().unwrap_or("").to_owned();
            if value.is_empty() || value == SECRET_REDACTED {
                continue;
            }
            match self.inner.secrets.set(secret_name, value.as_bytes()).await {
                Ok(()) => {
                    settings["bitbucket"][field] = Value::String(SECRET_REDACTED.to_owned());
                    moved = true;
                }
                Err(_) => tracing::warn!(field, "failed to move a Bitbucket token into the secret store"),
            }
        }
        (settings, moved)
    }

    /// `writeSettingsAtomically`: the sparse file (defaults stripped) plus a newline.
    async fn write_settings(&self, settings: &Value) -> Result<(), ServerSettingsError> {
        let contents = sparse_settings_json(settings);
        zc_core::write_file_string_atomically(&self.inner.settings_path, &contents)
            .await
            .map_err(|error| self.error(ServerSettingsOperation::WriteFile, Defect::from(&error)))
    }

    /// `materializeProviderEnvironmentSecrets`: markers become the stored values.
    pub async fn materialize(&self, settings: Value) -> LoadResult {
        let mut settings = settings;
        if let Some(instances) = settings.get_mut("providerInstances").and_then(Value::as_object_mut) {
            for (instance_id, instance) in instances.iter_mut() {
                let Some(Value::Array(environment)) = instance.get_mut("environment") else {
                    continue;
                };
                for variable in environment.iter_mut() {
                    if variable.get("sensitive") != Some(&Value::Bool(true)) || variable.get("valueRedacted") != Some(&Value::Bool(true)) {
                        continue;
                    }
                    let name = variable["name"].as_str().unwrap_or("").to_owned();
                    let secret = self
                        .inner
                        .secrets
                        .get(&logic::provider_environment_secret_name(instance_id, &name))
                        .await
                        .map_err(|cause| with_secret_context(self.error(ServerSettingsOperation::ReadSecret, cause), Some(instance_id), Some(&name)))?;
                    variable["value"] = Value::String(decode_secret(secret));
                }
            }
        }
        if let Some(sources) = settings.get_mut("usageLimitSources").and_then(Value::as_object_mut) {
            for (source_id, source) in sources.iter_mut() {
                if source["managementKey"].as_str() != Some(SECRET_REDACTED) {
                    continue;
                }
                let secret = self
                    .inner
                    .secrets
                    .get(&logic::usage_limit_source_secret_name(source_id))
                    .await
                    .map_err(|cause| self.error(ServerSettingsOperation::ReadSecret, cause))?;
                source["managementKey"] = Value::String(decode_secret(secret));
            }
        }
        for (field, secret_name) in BITBUCKET_SECRET_FIELDS {
            if settings["bitbucket"][field].as_str() != Some(SECRET_REDACTED) {
                continue;
            }
            let secret = self
                .inner
                .secrets
                .get(secret_name)
                .await
                .map_err(|cause| self.error(ServerSettingsOperation::ReadSecret, cause))?;
            settings["bitbucket"][field] = Value::String(decode_secret(secret));
        }
        Ok(settings)
    }

    /// `getSettings`: current settings with secrets materialized (internal consumers need the
    /// real values; RPCs redact with [`logic::redact_server_settings_for_client`]).
    pub async fn get_settings_value(&self) -> LoadResult {
        let settings = self.cached().await?;
        let settings = self.materialize(settings).await?;
        Ok(logic::resolve_text_generation_provider(settings))
    }

    /// `updateSettings(patch)` with the wire patch (decoded here, so its record order is kept).
    pub async fn update_settings_value(&self, raw_patch: &Value) -> LoadResult {
        let patch = decode_patch(raw_patch).map_err(|issue| settings_error("<memory>", ServerSettingsOperation::Normalize, defect(issue.message)))?;
        self.update_with_decoded_patch(&patch).await
    }

    async fn update_with_decoded_patch(&self, patch: &Value) -> LoadResult {
        let _permit = self.inner.write_lock.lock().await;
        let current = self.cached().await?;
        let updated = logic::apply_server_settings_patch(&current, patch);
        let (persisted, changes) = logic::persist_provider_environment_secrets(&current, updated);
        let next = normalize_server_settings(&persisted)?;
        let applied = self.apply_secret_changes(&changes).await?;
        let materialized = match self.materialize(next.clone()).await {
            Ok(materialized) => materialized,
            Err(error) => {
                self.rollback(applied).await;
                return Err(error);
            }
        };
        if let Err(error) = self.write_settings(&next).await {
            self.rollback(applied).await;
            return Err(error);
        }
        self.set_cache(next.clone()).await;
        self.inner.changes.publish(next);
        Ok(logic::resolve_text_generation_provider(materialized))
    }

    /// `applyProviderEnvironmentSecretChanges`: apply in order, remembering each previous value;
    /// on failure roll back what was applied (including the failing one: a store may mutate
    /// before it reports an error).
    async fn apply_secret_changes(&self, changes: &[SecretChange]) -> Result<Vec<AppliedSecret>, ServerSettingsError> {
        let mut applied = Vec::new();
        for change in changes {
            let previous = match self.inner.secrets.get(&change.secret_name).await {
                Ok(previous) => previous,
                Err(cause) => {
                    self.rollback(applied).await;
                    return Err(self.secret_error(ServerSettingsOperation::ReadSecret, change, cause));
                }
            };
            applied.push(AppliedSecret {
                change: change.clone(),
                previous,
            });
            let result = match &change.kind {
                SecretChangeKind::Write(value) => self.inner.secrets.set(&change.secret_name, value).await,
                SecretChangeKind::Remove(_) => self.inner.secrets.remove(&change.secret_name).await,
            };
            if let Err(cause) = result {
                let operation = match change.kind {
                    SecretChangeKind::Write(_) => ServerSettingsOperation::WriteSecret,
                    SecretChangeKind::Remove("remove-stale-secret") => ServerSettingsOperation::RemoveStaleSecret,
                    SecretChangeKind::Remove(_) => ServerSettingsOperation::RemoveSecret,
                };
                self.rollback(applied).await;
                return Err(self.secret_error(operation, change, cause));
            }
        }
        Ok(applied)
    }

    fn secret_error(&self, operation: ServerSettingsOperation, change: &SecretChange, cause: Defect) -> ServerSettingsError {
        with_secret_context(
            self.error(operation, cause),
            change.provider_instance_id.as_deref(),
            change.environment_variable.as_deref(),
        )
    }

    /// `rollbackProviderEnvironmentSecretWrites`: newest first, failures only logged.
    async fn rollback(&self, applied: Vec<AppliedSecret>) {
        for write in applied.into_iter().rev() {
            let result = match &write.previous {
                Some(previous) => self.inner.secrets.set(&write.change.secret_name, previous).await,
                None => self.inner.secrets.remove(&write.change.secret_name).await,
            };
            if let Err(cause) = result {
                tracing::warn!(
                    provider_instance_id = ?write.change.provider_instance_id,
                    environment_variable = ?write.change.environment_variable,
                    %cause,
                    "failed to roll back provider environment secret"
                );
            }
        }
    }

    /// `subscribeChanges`: every later change, materialized (a materialization failure is
    /// logged and the unmaterialized value goes out), text generation provider resolved.
    pub fn subscribe_changes_value(&self) -> futures::stream::BoxStream<'static, Value> {
        let subscription = self.inner.changes.subscribe();
        let service = self.clone();
        subscription
            .then(move |settings| {
                let service = service.clone();
                async move {
                    let materialized = match service.materialize(settings.clone()).await {
                        Ok(materialized) => materialized,
                        Err(error) => {
                            tracing::warn!(
                                operation = error.operation.as_str(),
                                provider_instance_id = ?error.provider_instance_id,
                                environment_variable = ?error.environment_variable,
                                "failed to materialize provider environment secrets"
                            );
                            settings
                        }
                    };
                    logic::resolve_text_generation_provider(materialized)
                }
            })
            .boxed()
    }
}

struct AppliedSecret {
    change: SecretChange,
    previous: Option<Vec<u8>>,
}

fn decode_secret(secret: Option<Vec<u8>>) -> String {
    secret.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()).unwrap_or_default()
}

/// Re-decode to restore canonical key order after logic that appends keys (decoding a canonical
/// value is the identity, so this only reorders).
fn canonical(settings: Value) -> Value {
    decode_settings(&settings).unwrap_or(settings)
}

/// `normalizeServerSettings`: encode → decode, fold in-config enabled flags, re-derive the
/// legacy project maps.
pub fn normalize_server_settings(settings: &Value) -> LoadResult {
    let mut next = decode_settings(settings).map_err(|issue| settings_error("<memory>", ServerSettingsOperation::Normalize, defect(issue.message)))?;
    logic::fold_provider_instance_enabled_flags(&mut next);
    Ok(canonical(logic::with_derived_legacy_overrides(next)))
}

/// The file contents `writeSettingsAtomically` writes for `settings`.
pub fn sparse_settings_json(settings: &Value) -> String {
    let sparse =
        logic::strip_default_server_settings(settings, Some(&logic::persisted_server_settings_defaults())).unwrap_or_else(|| Value::Object(Map::new()));
    format!("{}\n", stringify_pretty(&sparse))
}

/// `ServerSettingsService.layerTest(overrides)`: in memory, no file, no secrets.
pub fn test_settings(overrides: &Value) -> Value {
    let merged = crate::js::deep_merge(&default_settings(), overrides);
    normalize_server_settings(&merged).unwrap_or_else(|_| default_settings())
}

#[async_trait]
impl zc_ports::SettingsService for ServerSettingsService {
    async fn get_settings(&self) -> Result<ServerSettings, ServerSettingsError> {
        let value = self.get_settings_value().await?;
        to_typed_settings(&value).map_err(|issue| settings_error_at(&self.inner.settings_path, ServerSettingsOperation::Normalize, defect(issue.message)))
    }

    async fn update_settings(&self, patch: ServerSettingsPatch) -> Result<ServerSettings, ServerSettingsError> {
        let raw = serde_json::to_value(patch).map_err(|error| settings_error("<memory>", ServerSettingsOperation::Normalize, defect(error.to_string())))?;
        let value = self.update_settings_value(&raw).await?;
        to_typed_settings(&value).map_err(|issue| settings_error("<memory>", ServerSettingsOperation::Normalize, defect(issue.message)))
    }

    fn subscribe_changes(&self) -> zc_ports::EventStream<ServerSettings> {
        self.subscribe_changes_value()
            .filter_map(|value| async move { to_typed_settings(&value).ok() })
            .boxed()
    }
}
