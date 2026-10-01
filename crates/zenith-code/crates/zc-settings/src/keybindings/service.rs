//! The `Keybindings` service (`apps/server/src/keybindings.ts`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::watch;
use zc_contracts::{
    JsNumber, KeybindingsConfigError, LitKeybindingsInvalidEntry, LitKeybindingsMalformedConfig, ResolvedKeybindingRule, ServerConfigIssue,
    ServerConfigIssueKeybindingsInvalidEntry, ServerConfigIssueKeybindingsMalformedConfig, ServerRemoveKeybindingInput, ServerUpsertKeybindingInput,
};
use zc_core::{Defect, PubSub, Subscription};

use super::rules::{
    compile_resolved_keybindings_config, decode_keybinding_rule, default_keybindings, has_same_shortcut_context, is_same_keybinding_rule,
    merge_with_default_keybindings, resolve_keybinding_rule, KeybindingRule, MAX_KEYBINDINGS_COUNT,
};
use crate::errors::keybindings_error;
use crate::js::{js_trim, stringify_pretty};
use crate::watch::{file_filter, watch_directory, DirWatch, WATCH_DEBOUNCE};

/// `KeybindingsConfigState` / `KeybindingsChangeEvent`: the resolved rules and the non-fatal
/// configuration issues.
#[derive(Debug, Clone, PartialEq)]
pub struct KeybindingsConfigState {
    pub keybindings: Vec<ResolvedKeybindingRule>,
    pub issues: Vec<ServerConfigIssue>,
}

type StateResult = Result<KeybindingsConfigState, KeybindingsConfigError>;

struct Inner {
    config_path: PathBuf,
    /// `upsertSemaphore`.
    lock: tokio::sync::Mutex<()>,
    /// `resolvedConfigCache` (a failed load stays cached until invalidated, like Effect `Cache`).
    cache: tokio::sync::Mutex<Option<StateResult>>,
    changes: PubSub<KeybindingsConfigState>,
    started: tokio::sync::OnceCell<Result<(), KeybindingsConfigError>>,
    ready: watch::Sender<Option<Result<(), KeybindingsConfigError>>>,
    watcher: Mutex<Option<DirWatch>>,
}

/// The keybindings service. Cheap to clone.
#[derive(Clone)]
pub struct KeybindingsService {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for KeybindingsService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeybindingsService")
            .field("config_path", &self.inner.config_path)
            .finish_non_exhaustive()
    }
}

fn trim_issue_message(message: &str) -> String {
    let trimmed = js_trim(message);
    if trimmed.is_empty() {
        "Invalid keybindings configuration.".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn malformed_config_issue(detail: &str) -> ServerConfigIssue {
    ServerConfigIssue::KeybindingsMalformedConfig(ServerConfigIssueKeybindingsMalformedConfig {
        kind: LitKeybindingsMalformedConfig,
        message: trim_issue_message(detail),
    })
}

fn invalid_entry_issue(index: usize, detail: &str) -> ServerConfigIssue {
    ServerConfigIssue::KeybindingsInvalidEntry(ServerConfigIssueKeybindingsInvalidEntry {
        kind: LitKeybindingsInvalidEntry,
        message: trim_issue_message(detail),
        index: JsNumber::from(index as i64),
    })
}

/// `fromLenientJson(Schema.Array(Schema.Unknown))`: the entries, or the `Cause.pretty` text.
fn parse_entries(raw: &str) -> Result<Vec<Value>, String> {
    match zc_core::parse_lenient_json(raw) {
        Ok(Value::Array(entries)) => Ok(entries),
        Ok(_) => Err("SchemaError: Expected array".to_owned()),
        Err(_) => Err("SchemaError: Expected a valid JSON string".to_owned()),
    }
}

/// The file contents for `rules` (`fromJsonStringPretty(KeybindingsConfig)` plus a newline).
pub fn keybindings_config_json(rules: &[KeybindingRule]) -> Option<String> {
    if rules.len() > MAX_KEYBINDINGS_COUNT {
        return None;
    }
    let encoded = Value::Array(rules.iter().map(KeybindingRule::to_json).collect());
    Some(format!("{}\n", stringify_pretty(&encoded)))
}

fn rule_from_parts(key: &str, command: &zc_contracts::KeybindingCommand, when: Option<&str>) -> KeybindingRule {
    let command = serde_json::to_value(command)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    KeybindingRule {
        key: js_trim(key).to_owned(),
        command,
        when: when.map(|when| js_trim(when).to_owned()),
    }
}

impl KeybindingsService {
    /// `layer`: the service for `keybindings_config_path`. Nothing is read until first use.
    pub fn new(keybindings_config_path: impl Into<PathBuf>) -> Self {
        let (ready, _) = watch::channel(None);
        Self {
            inner: Arc::new(Inner {
                config_path: keybindings_config_path.into(),
                lock: tokio::sync::Mutex::new(()),
                cache: tokio::sync::Mutex::new(None),
                changes: PubSub::new(),
                started: tokio::sync::OnceCell::new(),
                ready,
                watcher: Mutex::new(None),
            }),
        }
    }

    /// `keybindings.json`.
    pub fn config_path(&self) -> &Path {
        &self.inner.config_path
    }

    fn error(&self, detail: &str, cause: Option<Defect>) -> KeybindingsConfigError {
        keybindings_error(&self.inner.config_path, detail, cause)
    }

    async fn exists(&self) -> Result<bool, KeybindingsConfigError> {
        tokio::fs::try_exists(&self.inner.config_path)
            .await
            .map_err(|error| self.error("failed to access keybindings config", Some(Defect::from(&error))))
    }

    async fn read_raw(&self) -> Result<String, KeybindingsConfigError> {
        tokio::fs::read_to_string(&self.inner.config_path)
            .await
            .map_err(|error| self.error("failed to read keybindings config", Some(Defect::from(&error))))
    }

    /// `loadWritableCustomKeybindingsConfig`: the valid rules of the file (invalid entries are
    /// dropped with a warning); a file that is not an array is an error, so it is never
    /// overwritten.
    async fn load_writable_custom_config(&self) -> Result<Vec<KeybindingRule>, KeybindingsConfigError> {
        if !self.exists().await? {
            return Ok(Vec::new());
        }
        let raw = self.read_raw().await?;
        let entries = parse_entries(&raw).map_err(|pretty| self.error("expected JSON array", Some(Defect::error("SchemaError", pretty))))?;
        let mut rules = Vec::new();
        for entry in entries {
            match decode_keybinding_rule(&entry).and_then(|rule| resolve_keybinding_rule(&rule).map(|_| rule)) {
                Ok(rule) => rules.push(rule),
                Err(error) => tracing::warn!(
                    path = %self.inner.config_path.display(),
                    %error,
                    "ignoring invalid keybinding entry"
                ),
            }
        }
        Ok(rules)
    }

    /// `loadRuntimeCustomKeybindingsConfig`: the valid rules and an issue per problem.
    async fn load_runtime_custom_config(&self) -> Result<(Vec<KeybindingRule>, Vec<ServerConfigIssue>), KeybindingsConfigError> {
        if !self.exists().await? {
            return Ok((Vec::new(), Vec::new()));
        }
        let raw = self.read_raw().await?;
        let entries = match parse_entries(&raw) {
            Ok(entries) => entries,
            Err(pretty) => {
                let detail = format!("expected JSON array ({pretty})");
                return Ok((Vec::new(), vec![malformed_config_issue(&detail)]));
            }
        };
        let mut rules = Vec::new();
        let mut issues = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            match decode_keybinding_rule(entry).and_then(|rule| resolve_keybinding_rule(&rule).map(|_| rule)) {
                Ok(rule) => rules.push(rule),
                Err(detail) => {
                    tracing::warn!(
                        path = %self.inner.config_path.display(),
                        index,
                        error = %detail,
                        "ignoring invalid keybinding entry"
                    );
                    issues.push(invalid_entry_issue(index, &detail));
                }
            }
        }
        Ok((rules, issues))
    }

    async fn write_config(&self, rules: &[KeybindingRule]) -> Result<(), KeybindingsConfigError> {
        let contents = keybindings_config_json(rules).ok_or_else(|| {
            self.error(
                "failed to write keybindings config",
                Some(Defect::error(
                    "SchemaError",
                    format!("Expected a value with a length of at most {MAX_KEYBINDINGS_COUNT}"),
                )),
            )
        })?;
        zc_core::write_file_string_atomically(&self.inner.config_path, &contents)
            .await
            .map_err(|error| self.error("failed to write keybindings config", Some(Defect::from(&error))))
    }

    async fn load_state_from_disk(&self) -> StateResult {
        let (rules, issues) = self.load_runtime_custom_config().await?;
        Ok(KeybindingsConfigState {
            keybindings: merge_with_default_keybindings(compile_resolved_keybindings_config(&rules)),
            issues,
        })
    }

    async fn invalidate(&self) {
        *self.inner.cache.lock().await = None;
    }

    /// `loadConfigState` / `getSnapshot`: the cached state, loading it on a miss.
    pub async fn load_config_state(&self) -> StateResult {
        let mut cache = self.inner.cache.lock().await;
        if let Some(result) = cache.as_ref() {
            return result.clone();
        }
        let result = self.load_state_from_disk().await;
        *cache = Some(result.clone());
        result
    }

    /// `revalidateAndEmit`.
    pub async fn revalidate_and_emit(&self) -> Result<(), KeybindingsConfigError> {
        let _permit = self.inner.lock.lock().await;
        self.invalidate().await;
        let state = self.load_config_state().await?;
        self.inner.changes.publish(state);
        Ok(())
    }

    /// `syncDefaultKeybindingsOnStartup`: create the file with the defaults, or append the
    /// defaults for commands the file does not bind (never evicting user rules, skipping a
    /// default whose shortcut context is taken, and leaving a file with issues alone).
    pub async fn sync_default_keybindings_on_startup(&self) -> Result<(), KeybindingsConfigError> {
        let _permit = self.inner.lock.lock().await;
        let result = self.sync_defaults_locked().await;
        self.invalidate().await;
        result
    }

    async fn sync_defaults_locked(&self) -> Result<(), KeybindingsConfigError> {
        let defaults = default_keybindings();
        if !self.exists().await? {
            return self.write_config(&defaults).await;
        }
        let (custom, issues) = self.load_runtime_custom_config().await?;
        if !issues.is_empty() {
            tracing::warn!(
                path = %self.inner.config_path.display(),
                issues = issues.len(),
                "skipping startup keybindings default sync because config has issues"
            );
            return Ok(());
        }
        let existing: Vec<&str> = custom.iter().map(|rule| rule.command.as_str()).collect();
        let mut missing = Vec::new();
        for default in &defaults {
            if existing.contains(&default.command.as_str()) {
                continue;
            }
            if let Some(conflict) = custom.iter().find(|entry| has_same_shortcut_context(entry, default)) {
                tracing::warn!(
                    path = %self.inner.config_path.display(),
                    default_command = %default.command,
                    conflicting_command = %conflict.command,
                    key = %default.key,
                    when = ?default.when,
                    reason = "shortcut context already used by existing rule",
                    "skipping default keybinding due to shortcut conflict"
                );
                continue;
            }
            missing.push(default.clone());
        }
        if missing.is_empty() {
            return Ok(());
        }
        let matching: Vec<&str> = defaults
            .iter()
            .filter(|default| custom.iter().any(|entry| is_same_keybinding_rule(entry, default)))
            .map(|default| default.command.as_str())
            .collect();
        if !matching.is_empty() {
            tracing::warn!(
                path = %self.inner.config_path.display(),
                commands = ?matching,
                "default keybinding rule already defined in user config"
            );
        }
        let available = MAX_KEYBINDINGS_COUNT.saturating_sub(custom.len());
        let skipped: Vec<&str> = missing.iter().skip(available).map(|rule| rule.command.as_str()).collect();
        if !skipped.is_empty() {
            tracing::warn!(
                path = %self.inner.config_path.display(),
                max_entries = MAX_KEYBINDINGS_COUNT,
                commands = ?skipped,
                "skipping default keybinding backfill at max entries"
            );
        }
        let appended: Vec<KeybindingRule> = missing.into_iter().take(available).collect();
        if appended.is_empty() {
            return Ok(());
        }
        let mut next = custom;
        next.extend(appended);
        self.write_config(&next).await
    }

    /// `start`: directory, watcher, default sync, first load. Later calls await the first.
    pub async fn start(&self) -> Result<(), KeybindingsConfigError> {
        let outcome = self
            .inner
            .started
            .get_or_init(|| async {
                let outcome = async {
                    self.start_watcher().await?;
                    self.sync_default_keybindings_on_startup().await?;
                    self.invalidate().await;
                    self.load_config_state().await.map(|_| ())
                }
                .await;
                self.inner.ready.send_replace(Some(outcome.clone()));
                outcome
            })
            .await;
        outcome.clone()
    }

    /// `ready`.
    pub async fn ready(&self) -> Result<(), KeybindingsConfigError> {
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

    async fn start_watcher(&self) -> Result<(), KeybindingsConfigError> {
        let path = &self.inner.config_path;
        let directory = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        tokio::fs::create_dir_all(&directory)
            .await
            .map_err(|error| self.error("failed to prepare keybindings config directory", Some(Defect::from(&error))))?;
        let service = self.clone();
        let handler: crate::watch::ChangeHandler = Arc::new(move || {
            let service = service.clone();
            Box::pin(async move {
                if let Err(error) = service.revalidate_and_emit().await {
                    tracing::warn!(detail = %error.detail, "keybindings reload failed");
                }
            })
        });
        match watch_directory(&directory, file_filter(path), WATCH_DEBOUNCE, handler) {
            Ok(watch) => *self.inner.watcher.lock().unwrap_or_else(|p| p.into_inner()) = Some(watch),
            Err(error) => tracing::warn!(%error, "could not watch the keybindings directory"),
        }
        Ok(())
    }

    /// `streamChanges`: every later change (eager subscription).
    pub fn subscribe_changes(&self) -> Subscription<KeybindingsConfigState> {
        self.inner.changes.subscribe()
    }

    async fn commit(&self, rules: &[KeybindingRule]) -> Result<Vec<ResolvedKeybindingRule>, KeybindingsConfigError> {
        self.write_config(rules).await?;
        let resolved = merge_with_default_keybindings(compile_resolved_keybindings_config(rules));
        let state = KeybindingsConfigState {
            keybindings: resolved.clone(),
            issues: Vec::new(),
        };
        *self.inner.cache.lock().await = Some(Ok(state.clone()));
        self.inner.changes.publish(state);
        Ok(resolved)
    }

    /// `upsertKeybindingRule`: replace the identical rule (or `replace`'s target) by the new
    /// one, appended last; keep the newest 256.
    pub async fn upsert_keybinding_rule(&self, input: &ServerUpsertKeybindingInput) -> Result<Vec<ResolvedKeybindingRule>, KeybindingsConfigError> {
        let _permit = self.inner.lock.lock().await;
        let custom = self.load_writable_custom_config().await?;
        let rule = rule_from_parts(&input.key, &input.command, input.when.as_deref());
        let replace = input
            .replace
            .as_ref()
            .map(|target| rule_from_parts(&target.key, &target.command, target.when.as_deref()));
        let mut next: Vec<KeybindingRule> = custom
            .into_iter()
            .filter(|entry| !is_same_keybinding_rule(entry, &rule) && replace.as_ref().is_none_or(|target| !is_same_keybinding_rule(entry, target)))
            .collect();
        next.push(rule);
        if next.len() > MAX_KEYBINDINGS_COUNT {
            tracing::warn!(
                path = %self.inner.config_path.display(),
                max_entries = MAX_KEYBINDINGS_COUNT,
                "truncating keybindings config to max entries"
            );
            next.drain(..next.len() - MAX_KEYBINDINGS_COUNT);
        }
        self.commit(&next).await
    }

    /// `removeKeybindingRule`: drop the rule with exactly this key, command and `when`.
    pub async fn remove_keybinding_rule(&self, input: &ServerRemoveKeybindingInput) -> Result<Vec<ResolvedKeybindingRule>, KeybindingsConfigError> {
        let _permit = self.inner.lock.lock().await;
        let custom = self.load_writable_custom_config().await?;
        let target = rule_from_parts(&input.key, &input.command, input.when.as_deref());
        let next: Vec<KeybindingRule> = custom.into_iter().filter(|entry| !is_same_keybinding_rule(entry, &target)).collect();
        self.commit(&next).await
    }
}
