//! Port of `Layers/ProviderSessionDirectory.ts`: which provider instance owns each thread, with
//! its resume cursor and runtime payload, persisted in `provider_session_runtime`.

use serde_json::{Map, Value};
use zc_contracts::{ProviderDriverKind, ProviderInstanceId, RuntimeMode, ThreadId};
use zc_db::repos::provider_session_runtime::{self as repo, OnConflict, ProviderSessionRuntime};
use zc_db::Db;

use crate::errors::ProviderServiceError;

/// `ProviderSessionRuntimeStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStatus {
    Starting,
    Running,
    Stopped,
    Error,
}

impl RuntimeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Error => "error",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "starting" => Some(Self::Starting),
            "running" => Some(Self::Running),
            "stopped" => Some(Self::Stopped),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

/// `ProviderRuntimeBinding`. On writes, `None` fields keep the stored value; `Some(Value::Null)`
/// clears a JSON field. On reads, a SQL `NULL` cursor or payload is `Some(Value::Null)`, as in TS.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderRuntimeBinding {
    pub thread_id: ThreadId,
    pub provider: ProviderDriverKind,
    /// Required on writes (the persistence layer promotes legacy `NULL` rows on read).
    pub provider_instance_id: Option<ProviderInstanceId>,
    pub adapter_key: Option<String>,
    pub status: Option<RuntimeStatus>,
    pub resume_cursor: Option<Value>,
    pub runtime_payload: Option<Value>,
    pub runtime_mode: Option<RuntimeMode>,
}

impl ProviderRuntimeBinding {
    /// A write touching nothing but the routing keys.
    pub fn new(thread_id: ThreadId, provider: ProviderDriverKind, provider_instance_id: ProviderInstanceId) -> Self {
        Self {
            thread_id,
            provider,
            provider_instance_id: Some(provider_instance_id),
            adapter_key: None,
            status: None,
            resume_cursor: None,
            runtime_payload: None,
            runtime_mode: None,
        }
    }

    /// `resumeCursor !== null && resumeCursor !== undefined`.
    pub fn has_resume_cursor(&self) -> bool {
        is_present(&self.resume_cursor)
    }
}

/// `value !== null && value !== undefined`.
pub fn is_present(value: &Option<Value>) -> bool {
    matches!(value, Some(value) if !value.is_null())
}

/// `ProviderRuntimeBindingWithMetadata`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderRuntimeBindingWithMetadata {
    pub binding: ProviderRuntimeBinding,
    pub last_seen_at: String,
}

fn runtime_mode_str(mode: RuntimeMode) -> &'static str {
    match mode {
        RuntimeMode::ApprovalRequired => "approval-required",
        RuntimeMode::AutoAcceptEdits => "auto-accept-edits",
        RuntimeMode::Auto => "auto",
        RuntimeMode::FullAccess => "full-access",
    }
}

fn parse_runtime_mode(value: &str) -> Option<RuntimeMode> {
    serde_json::from_value(Value::String(value.to_owned())).ok()
}

fn persistence(operation: &str) -> impl Fn(zc_db::DbError) -> ProviderServiceError + '_ {
    move |error| {
        tracing::debug!(%error, operation, "provider session directory");
        ProviderServiceError::persistence(operation, format!("Failed to execute {operation}."))
    }
}

/// `ProviderSlug` check of `ProviderDriverKind` (`^[a-zA-Z][a-zA-Z0-9_-]*$`, ≤ 64).
fn is_driver_kind(value: &str) -> bool {
    let mut chars = value.chars();
    value.len() <= 64 && chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn to_binding(runtime: ProviderSessionRuntime, operation: &str) -> Result<ProviderRuntimeBindingWithMetadata, ProviderServiceError> {
    if !is_driver_kind(runtime.provider_name.trim()) {
        return Err(ProviderServiceError::persistence(
            operation,
            format!("Unknown persisted provider '{}'.", runtime.provider_name),
        ));
    }
    let provider = ProviderDriverKind::from(runtime.provider_name.as_str());
    Ok(ProviderRuntimeBindingWithMetadata {
        binding: ProviderRuntimeBinding {
            thread_id: ThreadId::from(runtime.thread_id.as_str()),
            // Rows from before the instance split: promote to the driver's default instance.
            provider_instance_id: Some(ProviderInstanceId::from(runtime.provider_instance_id.as_deref().unwrap_or(provider.as_str()))),
            provider,
            adapter_key: Some(runtime.adapter_key),
            status: RuntimeStatus::parse(&runtime.status),
            resume_cursor: Some(runtime.resume_cursor.unwrap_or(Value::Null)),
            runtime_payload: Some(runtime.runtime_payload.unwrap_or(Value::Null)),
            runtime_mode: parse_runtime_mode(&runtime.runtime_mode),
        },
        last_seen_at: runtime.last_seen_at,
    })
}

/// `mergeRuntimePayload(existing, next)`: objects merge shallowly, anything else replaces.
pub fn merge_runtime_payload(existing: Option<&Value>, next: Option<&Value>) -> Value {
    match next {
        None => existing.cloned().unwrap_or(Value::Null),
        Some(Value::Object(next)) => match existing {
            Some(Value::Object(existing)) => {
                let mut merged: Map<String, Value> = existing.clone();
                for (key, value) in next {
                    merged.insert(key.clone(), value.clone());
                }
                Value::Object(merged)
            }
            _ => Value::Object(next.clone()),
        },
        Some(other) => other.clone(),
    }
}

fn non_null(value: Value) -> Option<Value> {
    (!value.is_null()).then_some(value)
}

/// `ProviderSessionDirectory`.
#[derive(Clone)]
pub struct ProviderSessionDirectory {
    db: Db,
}

impl ProviderSessionDirectory {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// `upsert(binding, {onConflict})`.
    pub async fn upsert(&self, binding: ProviderRuntimeBinding, on_conflict: OnConflict) -> Result<(), ProviderServiceError> {
        let thread_id = binding.thread_id.to_string();
        let existing = self
            .db
            .call(move |conn| repo::get_by_thread_id(conn, &thread_id))
            .await
            .map_err(persistence("ProviderSessionDirectory.upsert:getByThreadId"))?;
        if binding.thread_id.as_str().is_empty() {
            return Err(ProviderServiceError::validation(
                "ProviderSessionDirectory.upsert",
                "threadId must be a non-empty string.",
            ));
        }
        let provider_changed = existing.as_ref().is_some_and(|existing| existing.provider_name != binding.provider.as_str());
        let provider_instance_id = binding.provider_instance_id.as_ref().map(ToString::to_string).or_else(|| {
            if provider_changed {
                None
            } else {
                existing.as_ref().and_then(|existing| existing.provider_instance_id.clone())
            }
        });
        let Some(provider_instance_id) = provider_instance_id else {
            return Err(ProviderServiceError::validation(
                "ProviderSessionDirectory.upsert",
                "providerInstanceId is required for provider session runtime bindings.",
            ));
        };
        let provider = binding.provider.to_string();
        let runtime = ProviderSessionRuntime {
            thread_id: binding.thread_id.to_string(),
            provider_instance_id: Some(provider_instance_id),
            adapter_key: binding.adapter_key.clone().unwrap_or_else(|| {
                if provider_changed {
                    provider.clone()
                } else {
                    existing
                        .as_ref()
                        .map(|existing| existing.adapter_key.clone())
                        .unwrap_or_else(|| provider.clone())
                }
            }),
            runtime_mode: binding
                .runtime_mode
                .map(|mode| runtime_mode_str(mode).to_owned())
                .or_else(|| existing.as_ref().map(|existing| existing.runtime_mode.clone()))
                .unwrap_or_else(|| "full-access".to_owned()),
            status: binding
                .status
                .map(|status| status.as_str().to_owned())
                .or_else(|| existing.as_ref().map(|existing| existing.status.clone()))
                .unwrap_or_else(|| "running".to_owned()),
            last_seen_at: zc_core::now_iso(),
            resume_cursor: match binding.resume_cursor {
                Some(cursor) => non_null(cursor),
                None => existing.as_ref().and_then(|existing| existing.resume_cursor.clone()),
            },
            runtime_payload: non_null(merge_runtime_payload(
                existing.as_ref().and_then(|existing| existing.runtime_payload.as_ref()),
                binding.runtime_payload.as_ref(),
            )),
            provider_name: provider,
        };
        self.db
            .call(move |conn| repo::upsert(conn, &runtime, on_conflict))
            .await
            .map_err(persistence("ProviderSessionDirectory.upsert:upsert"))
    }

    /// `recordImportedTranscript({threadId, source})`: keep the source file without touching the
    /// current session. No row, no change.
    pub async fn record_imported_transcript(&self, thread_id: &ThreadId, source: Value) -> Result<(), ProviderServiceError> {
        let thread_id = thread_id.to_string();
        self.db
            .call(move |conn| repo::record_imported_transcript(conn, &thread_id, &source))
            .await
            .map_err(persistence("ProviderSessionDirectory.recordImportedTranscript"))
    }

    /// `getBinding(threadId)`.
    pub async fn get_binding(&self, thread_id: &ThreadId) -> Result<Option<ProviderRuntimeBinding>, ProviderServiceError> {
        let id = thread_id.to_string();
        let runtime = self
            .db
            .call(move |conn| repo::get_by_thread_id(conn, &id))
            .await
            .map_err(persistence("ProviderSessionDirectory.getBinding:getByThreadId"))?;
        runtime
            .map(|runtime| to_binding(runtime, "ProviderSessionDirectory.getBinding").map(|with| with.binding))
            .transpose()
    }

    /// `getProvider(threadId)`.
    pub async fn get_provider(&self, thread_id: &ThreadId) -> Result<ProviderDriverKind, ProviderServiceError> {
        match self.get_binding(thread_id).await? {
            Some(binding) => Ok(binding.provider),
            None => Err(ProviderServiceError::persistence(
                "ProviderSessionDirectory.getProvider",
                format!("No persisted provider binding found for thread '{thread_id}'."),
            )),
        }
    }

    /// `listThreadIds()`.
    pub async fn list_thread_ids(&self) -> Result<Vec<ThreadId>, ProviderServiceError> {
        let rows = self
            .db
            .call(|conn| repo::list(conn, false))
            .await
            .map_err(persistence("ProviderSessionDirectory.listThreadIds:list"))?;
        Ok(rows.into_iter().map(|row| ThreadId::from(row.thread_id)).collect())
    }

    /// `listBindings({excludeStopped})`: oldest `lastSeenAt` first.
    pub async fn list_bindings(&self, exclude_stopped: bool) -> Result<Vec<ProviderRuntimeBindingWithMetadata>, ProviderServiceError> {
        let rows = self
            .db
            .call(move |conn| repo::list(conn, exclude_stopped))
            .await
            .map_err(persistence("ProviderSessionDirectory.listBindings:list"))?;
        rows.into_iter().map(|row| to_binding(row, "ProviderSessionDirectory.listBindings")).collect()
    }
}
