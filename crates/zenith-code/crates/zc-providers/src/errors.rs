//! `provider/Errors.ts`: what the provider service and the registries fail with. Each variant
//! keeps its TS `_tag` and declared fields so it crosses into orchestration activities and RPC
//! exits unchanged ([`ProviderServiceError::to_tagged`]).

use serde_json::Value;
use zc_ports::adapter::AdapterError;
use zc_ports::TaggedError;

/// `ProviderServiceError`.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ProviderServiceError {
    /// `ProviderValidationError`.
    #[error("Provider validation failed in {operation}: {issue}")]
    Validation { operation: String, issue: String },
    /// `ProviderUnsupportedError`.
    #[error("Provider '{provider}' is not implemented")]
    Unsupported { provider: String },
    /// `ProviderWorkspaceMissingError`.
    #[error("This thread's workspace folder no longer exists or is not a directory: {cwd}. Restore the folder at this path before retrying.")]
    WorkspaceMissing { thread_id: String, cwd: String },
    /// `ProviderInstanceNotFoundError`.
    #[error("No provider instance bound to id '{instance_id}'")]
    InstanceNotFound { instance_id: String },
    /// `ProviderSessionNotFoundError`.
    #[error("Unknown provider thread: {thread_id}")]
    SessionNotFound { thread_id: String },
    /// `ProviderSessionDirectoryPersistenceError`.
    #[error("Provider session directory persistence error in {operation}: {detail}")]
    DirectoryPersistence { operation: String, detail: String },
    /// One of the adapter errors (`ProviderAdapterError`).
    #[error(transparent)]
    Adapter(#[from] AdapterError),
}

impl ProviderServiceError {
    pub fn validation(operation: impl Into<String>, issue: impl Into<String>) -> Self {
        Self::Validation {
            operation: operation.into(),
            issue: issue.into(),
        }
    }

    pub fn persistence(operation: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::DirectoryPersistence {
            operation: operation.into(),
            detail: detail.into(),
        }
    }

    /// The `_tag` the TS error carries.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Validation { .. } => "ProviderValidationError",
            Self::Unsupported { .. } => "ProviderUnsupportedError",
            Self::WorkspaceMissing { .. } => "ProviderWorkspaceMissingError",
            Self::InstanceNotFound { .. } => "ProviderInstanceNotFoundError",
            Self::SessionNotFound { .. } => "ProviderSessionNotFoundError",
            Self::DirectoryPersistence { .. } => "ProviderSessionDirectoryPersistenceError",
            Self::Adapter(error) => adapter_error_tag(error),
        }
    }

    /// The wire-encoded error (`{"_tag", …fields}` plus the human message).
    pub fn to_tagged(&self) -> TaggedError {
        let message = self.to_string();
        match self {
            Self::Validation { operation, issue } => TaggedError::new(self.tag(), message)
                .with("operation", operation.as_str())
                .with("issue", issue.as_str()),
            Self::Unsupported { provider } => TaggedError::new(self.tag(), message).with("provider", provider.as_str()),
            Self::WorkspaceMissing { thread_id, cwd } => TaggedError::new(self.tag(), message)
                .with("threadId", thread_id.as_str())
                .with("cwd", cwd.as_str()),
            Self::InstanceNotFound { instance_id } => TaggedError::new(self.tag(), message).with("instanceId", instance_id.as_str()),
            Self::SessionNotFound { thread_id } => TaggedError::new(self.tag(), message).with("threadId", thread_id.as_str()),
            Self::DirectoryPersistence { operation, detail } => TaggedError::new(self.tag(), message)
                .with("operation", operation.as_str())
                .with("detail", detail.as_str()),
            Self::Adapter(error) => {
                let mut tagged = TaggedError::new(self.tag(), message);
                if let Ok(Value::Object(fields)) = serde_json::to_value(error) {
                    for (key, value) in fields {
                        if key != "_tag" {
                            tagged.fields.insert(key, value);
                        }
                    }
                }
                tagged
            }
        }
    }
}

impl PartialEq for ProviderServiceError {
    fn eq(&self, other: &Self) -> bool {
        self.to_tagged() == other.to_tagged()
    }
}

impl From<ProviderServiceError> for TaggedError {
    fn from(error: ProviderServiceError) -> Self {
        error.to_tagged()
    }
}

/// The `_tag` of an adapter error.
pub fn adapter_error_tag(error: &AdapterError) -> &'static str {
    match error {
        AdapterError::Validation { .. } => "ProviderAdapterValidationError",
        AdapterError::SessionNotFound { .. } => "ProviderAdapterSessionNotFoundError",
        AdapterError::SessionClosed { .. } => "ProviderAdapterSessionClosedError",
        AdapterError::Request { .. } => "ProviderAdapterRequestError",
        AdapterError::Process { .. } => "ProviderAdapterProcessError",
    }
}

/// `ProviderDriverError`: a driver could not build an instance (or one of its account-level
/// operations failed). The registry turns create failures into unavailable snapshots.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("Provider driver '{driver}' failed to create instance '{instance_id}': {detail}")]
pub struct ProviderDriverError {
    pub driver: String,
    pub instance_id: String,
    pub detail: String,
}

impl ProviderDriverError {
    pub fn new(driver: impl Into<String>, instance_id: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            driver: driver.into(),
            instance_id: instance_id.into(),
            detail: detail.into(),
        }
    }

    pub fn to_tagged(&self) -> TaggedError {
        TaggedError::new("ProviderDriverError", self.to_string())
            .with("driver", self.driver.as_str())
            .with("instanceId", self.instance_id.as_str())
            .with("detail", self.detail.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn errors_encode_like_the_ts_tagged_errors() {
        let error = ProviderServiceError::validation("ProviderService.sendTurn", "nope");
        assert_eq!(error.to_string(), "Provider validation failed in ProviderService.sendTurn: nope");
        assert_eq!(
            serde_json::to_value(error.to_tagged()).unwrap(),
            json!({"_tag": "ProviderValidationError", "operation": "ProviderService.sendTurn", "issue": "nope"})
        );
        let adapter = ProviderServiceError::from(AdapterError::Request {
            provider: "codex".into(),
            method: "thread/compact".into(),
            detail: "busy".into(),
        });
        assert_eq!(adapter.tag(), "ProviderAdapterRequestError");
        assert_eq!(
            serde_json::to_value(adapter.to_tagged()).unwrap(),
            json!({"_tag": "ProviderAdapterRequestError", "provider": "codex", "method": "thread/compact", "detail": "busy"})
        );
    }
}
