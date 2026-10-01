//! Error causes and the two contract errors of this module.
//!
//! TS keeps the original error object as `cause` and encodes it with `Schema.Defect()` when it
//! crosses the wire: an `Error` becomes `{"name": …, "message": …}` plus its own `cause`, nested
//! the same way; any other value passes through. [`Cause`] keeps the Rust error (so callers can
//! still match on it, like `error.cause._tag` in TS) and produces that encoding on demand.
//!
//! [`SourceControlProviderError`] and [`SourceControlRepositoryError`] serialize exactly like the
//! contracts (`zc_contracts::SourceControlProviderError` / `SourceControlRepositoryError`).

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use serde::{Serialize, Serializer};
use serde_json::{json, Map, Value};
use zc_contracts::{LitSourceControlProviderError, LitSourceControlRepositoryError, SourceControlProviderKind};
use zc_core::defect::Defect;
use zc_core::vcs_process::VcsProcessError;
use zc_vcs::errors::{GitCommandError, VcsError};

/// An error that can be kept as a `cause`.
pub trait CauseError: fmt::Debug + Send + Sync + 'static {
    /// The `Schema.Defect()` encoding of this error.
    fn defect(&self) -> Value;
    fn as_any(&self) -> &dyn Any;
}

/// `{"name", "message", "cause"?}`: how Effect encodes an `Error` defect.
pub fn error_defect(name: &str, message: impl Into<String>, cause: Option<Value>) -> Value {
    let mut map = Map::new();
    map.insert("name".into(), Value::String(name.to_owned()));
    map.insert("message".into(), Value::String(message.into()));
    if let Some(cause) = cause {
        map.insert("cause".into(), cause);
    }
    Value::Object(map)
}

/// A shared, encodable error cause.
#[derive(Clone)]
pub struct Cause(Arc<dyn CauseError>);

impl Cause {
    pub fn new<E: CauseError>(error: E) -> Self {
        Self(Arc::new(error))
    }

    /// A plain `Error` with a message (`new Error(message)`).
    pub fn message(message: impl Into<String>) -> Self {
        Self::new(Defect::error("Error", message.into()))
    }

    /// The wire encoding.
    pub fn defect(&self) -> Value {
        self.0.defect()
    }

    pub fn downcast_ref<T: 'static>(&self) -> Option<&T> {
        self.0.as_any().downcast_ref::<T>()
    }

    /// The `_tag`/`name` of the cause.
    pub fn name(&self) -> Option<String> {
        self.defect().get("name").and_then(Value::as_str).map(str::to_owned)
    }
}

impl fmt::Debug for Cause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl CauseError for Defect {
    fn defect(&self) -> Value {
        self.0.clone()
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl CauseError for VcsProcessError {
    fn defect(&self) -> Value {
        let cause = match self {
            VcsProcessError::Spawn { cause, .. } | VcsProcessError::StdinWrite { cause, .. } | VcsProcessError::OutputRead { cause, .. } => {
                Some(cause.0.clone())
            }
            _ => None,
        };
        error_defect(self.tag(), self.to_string(), cause)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl CauseError for GitCommandError {
    fn defect(&self) -> Value {
        error_defect("GitCommandError", self.message(), self.cause.as_ref().map(|c| c.0.clone()))
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl CauseError for VcsError {
    fn defect(&self) -> Value {
        let encoded = serde_json::to_value(self).unwrap_or(Value::Null);
        error_defect(self.tag(), self.message(), encoded.get("cause").cloned())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl CauseError for std::io::Error {
    fn defect(&self) -> Value {
        error_defect("Error", self.to_string(), None)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `SourceControlProviderError` (`contracts/sourceControl.ts`).
#[derive(Debug, Clone)]
pub struct SourceControlProviderError {
    pub provider: SourceControlProviderKind,
    pub operation: String,
    pub cwd: String,
    pub command: Option<String>,
    pub repository: Option<String>,
    pub reference: Option<String>,
    pub detail: String,
    pub cause: Option<Cause>,
}

impl SourceControlProviderError {
    pub fn new(provider: SourceControlProviderKind, operation: impl Into<String>, cwd: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            provider,
            operation: operation.into(),
            cwd: cwd.into(),
            command: None,
            repository: None,
            reference: None,
            detail: detail.into(),
            cause: None,
        }
    }

    pub fn with_command(mut self, command: impl Into<String>) -> Self {
        self.command = Some(command.into());
        self
    }

    pub fn with_repository(mut self, repository: impl Into<String>) -> Self {
        self.repository = Some(repository.into());
        self
    }

    pub fn with_reference(mut self, reference: impl Into<String>) -> Self {
        self.reference = Some(reference.into());
        self
    }

    pub fn with_cause(mut self, cause: Cause) -> Self {
        self.cause = Some(cause);
        self
    }

    /// The TS `message` getter.
    pub fn message(&self) -> String {
        format!(
            "Source control provider {} failed in {}: {}",
            self.provider.as_str(),
            self.operation,
            self.detail
        )
    }

    /// The contract value.
    pub fn to_wire(&self) -> zc_contracts::SourceControlProviderError {
        zc_contracts::SourceControlProviderError {
            tag: LitSourceControlProviderError,
            provider: self.provider,
            operation: self.operation.clone(),
            cwd: self.cwd.clone(),
            command: self.command.clone(),
            repository: self.repository.clone(),
            reference: self.reference.clone(),
            detail: self.detail.clone(),
            cause: self.cause.as_ref().map(Cause::defect),
        }
    }
}

impl fmt::Display for SourceControlProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for SourceControlProviderError {}

impl Serialize for SourceControlProviderError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_wire().serialize(serializer)
    }
}

impl CauseError for SourceControlProviderError {
    fn defect(&self) -> Value {
        error_defect("SourceControlProviderError", self.message(), self.cause.as_ref().map(Cause::defect))
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `SourceControlRepositoryError` (`contracts/sourceControl.ts`).
#[derive(Debug, Clone)]
pub struct SourceControlRepositoryError {
    pub provider: SourceControlProviderKind,
    pub operation: String,
    pub detail: String,
    pub cause: Option<Cause>,
}

impl SourceControlRepositoryError {
    pub fn new(operation: impl Into<String>, provider: SourceControlProviderKind, detail: impl Into<String>) -> Self {
        Self {
            provider,
            operation: operation.into(),
            detail: detail.into(),
            cause: None,
        }
    }

    pub fn with_cause(mut self, cause: Cause) -> Self {
        self.cause = Some(cause);
        self
    }

    /// The TS `message` getter.
    pub fn message(&self) -> String {
        format!(
            "Source control repository operation {} failed for {}: {}",
            self.operation,
            self.provider.as_str(),
            self.detail
        )
    }

    pub fn to_wire(&self) -> zc_contracts::SourceControlRepositoryError {
        zc_contracts::SourceControlRepositoryError {
            tag: LitSourceControlRepositoryError,
            provider: self.provider,
            operation: self.operation.clone(),
            detail: self.detail.clone(),
            cause: self.cause.as_ref().map(Cause::defect),
        }
    }
}

impl fmt::Display for SourceControlRepositoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for SourceControlRepositoryError {}

impl Serialize for SourceControlRepositoryError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_wire().serialize(serializer)
    }
}

impl CauseError for SourceControlRepositoryError {
    fn defect(&self) -> Value {
        error_defect("SourceControlRepositoryError", self.message(), self.cause.as_ref().map(Cause::defect))
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Builds a tagged error's own wire encoding: `{"_tag": tag, …fields}` with a `cause` defect.
pub(crate) fn tagged(tag: &str, fields: Value, cause: Option<&Cause>) -> Value {
    let mut map = Map::new();
    map.insert("_tag".into(), json!(tag));
    if let Value::Object(fields) = fields {
        for (key, value) in fields {
            if !value.is_null() {
                map.insert(key, value);
            }
        }
    }
    if let Some(cause) = cause {
        map.insert("cause".into(), cause.defect());
    }
    Value::Object(map)
}
