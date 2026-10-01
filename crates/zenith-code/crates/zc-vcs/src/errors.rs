//! The tagged errors of the git and VCS RPCs, encoded exactly like the contracts
//! (`git.ts` `GitCommandError`, `GitManagerError`; `vcs.ts` `VcsError`; `review.ts`
//! `ReviewDiffPreviewError`).
//!
//! Each error keeps its human `message` (a getter in TS, never on the wire) through
//! [`std::fmt::Display`]. [`IntoTagged`] converts any of them into the zc-ports
//! [`TaggedError`] placeholder the port traits use.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use zc_core::defect::Defect;
use zc_core::vcs_process::VcsProcessError;
use zc_ports::TaggedError;

use crate::contracts::{RequestedVcsKind, VcsDriverKind};

/// The `Schema.Defect` encoding of an Effect `PlatformError` from a file-system call:
/// `{"name":"PlatformError","message":"<Reason>: FileSystem.<method> (<path>)"}` (Effect also
/// nests Node's own error as `cause`; that part is not reproduced).
pub fn platform_error_defect(method: &str, path: &str, io: &std::io::Error) -> Defect {
    use std::io::ErrorKind::*;
    let reason = match io.kind() {
        NotFound => "NotFound",
        PermissionDenied => "PermissionDenied",
        AlreadyExists => "AlreadyExists",
        NotADirectory | IsADirectory | InvalidInput => "BadResource",
        TimedOut => "TimedOut",
        WouldBlock => "WouldBlock",
        UnexpectedEof => "UnexpectedEof",
        InvalidData => "InvalidData",
        WriteZero => "WriteZero",
        ResourceBusy => "Busy",
        _ => "Unknown",
    };
    Defect::error("PlatformError", format!("{reason}: FileSystem.{method} ({path})"))
}

/// `GitCommandError` (`git.ts`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "_tag", rename = "GitCommandError", rename_all = "camelCase")]
pub struct GitCommandError {
    pub operation: String,
    pub command: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_length: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_length: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_length: Option<usize>,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<Defect>,
    /// Rust-only: the spawn failed because `cwd` does not exist or is not a directory
    /// (`isMissingGitCwdError`, which TS reads from the `PlatformError` cause).
    #[serde(skip)]
    pub missing_cwd: bool,
}

impl GitCommandError {
    /// `{operation, command, cwd, detail}` with nothing else.
    pub fn new(operation: impl Into<String>, command: impl Into<String>, cwd: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            operation: operation.into(),
            command: command.into(),
            cwd: cwd.into(),
            argument_count: None,
            exit_code: None,
            stdout_length: None,
            stderr_length: None,
            output_length: None,
            detail: detail.into(),
            cause: None,
            missing_cwd: false,
        }
    }

    /// `gitCommandContext({operation, cwd, args})` plus a detail: command `"git"` and the
    /// argument count.
    pub fn git(operation: &str, cwd: &str, argument_count: usize, detail: impl Into<String>) -> Self {
        Self {
            argument_count: Some(argument_count),
            ..Self::new(operation, "git", cwd, detail)
        }
    }

    pub fn with_cause(mut self, cause: Defect) -> Self {
        self.cause = Some(cause);
        self
    }

    pub fn message(&self) -> String {
        format!("Git command failed in {} ({}): {}", self.operation, self.cwd, self.detail)
    }
}

impl std::fmt::Display for GitCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for GitCommandError {}

/// `GitManagerError` (`git.ts`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "_tag", rename = "GitManagerError")]
pub struct GitManagerError {
    pub operation: String,
    pub cwd: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<Defect>,
}

impl GitManagerError {
    pub fn new(operation: impl Into<String>, cwd: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            operation: operation.into(),
            cwd: cwd.into(),
            detail: detail.into(),
            cause: None,
        }
    }

    pub fn with_cause(mut self, cause: Defect) -> Self {
        self.cause = Some(cause);
        self
    }

    pub fn message(&self) -> String {
        format!("Git manager failed in {}: {}", self.operation, self.detail)
    }
}

impl std::fmt::Display for GitManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for GitManagerError {}

/// `VcsRepositoryDetectionError` (`vcs.ts`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "_tag", rename = "VcsRepositoryDetectionError")]
pub struct VcsRepositoryDetectionError {
    pub operation: String,
    pub cwd: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<Defect>,
}

impl VcsRepositoryDetectionError {
    pub fn message(&self) -> String {
        format!("VCS repository detection failed in {}: {} - {}", self.operation, self.cwd, self.detail)
    }
}

/// `VcsUnsupportedOperationError` (`vcs.ts`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "_tag", rename = "VcsUnsupportedOperationError")]
pub struct VcsUnsupportedOperationError {
    pub operation: String,
    pub kind: VcsDriverKind,
    pub detail: String,
}

impl VcsUnsupportedOperationError {
    pub fn new(operation: impl Into<String>, kind: VcsDriverKind, detail: impl Into<String>) -> Self {
        Self {
            operation: operation.into(),
            kind,
            detail: detail.into(),
        }
    }

    pub fn message(&self) -> String {
        format!("VCS operation is unsupported for {} in {}: {}", self.kind, self.operation, self.detail)
    }
}

/// `VcsError` (`vcs.ts`): the process errors plus detection and unsupported-operation errors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum VcsError {
    Process(VcsProcessError),
    RepositoryDetection(VcsRepositoryDetectionError),
    UnsupportedOperation(VcsUnsupportedOperationError),
}

impl VcsError {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Process(error) => error.tag(),
            Self::RepositoryDetection(_) => "VcsRepositoryDetectionError",
            Self::UnsupportedOperation(_) => "VcsUnsupportedOperationError",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Process(error) => error.to_string(),
            Self::RepositoryDetection(error) => error.message(),
            Self::UnsupportedOperation(error) => error.message(),
        }
    }

    /// `new VcsProcessExitError({operation, command, cwd, exitCode, detail})`, the shape
    /// `GitVcsDriver.ts` builds by hand (no argument count, no failure kind).
    pub fn exit(operation: impl Into<String>, command: impl Into<String>, cwd: impl Into<String>, exit_code: i32, detail: impl Into<String>) -> Self {
        Self::Process(VcsProcessError::Exit {
            operation: operation.into(),
            command: command.into(),
            cwd: cwd.into(),
            argument_count: None,
            exit_code,
            detail: detail.into(),
            failure_kind: None,
            retryable: None,
            stderr_length: None,
            stderr_truncated: None,
        })
    }

    pub fn unsupported(operation: impl Into<String>, requested: RequestedVcsKind, detail: impl Into<String>) -> Self {
        let kind = match requested {
            RequestedVcsKind::Auto => VcsDriverKind::Unknown,
            RequestedVcsKind::Kind(kind) => kind,
        };
        Self::UnsupportedOperation(VcsUnsupportedOperationError::new(operation, kind, detail))
    }

    /// The `Schema.Defect` encoding of this error as a `cause`: `{name: _tag, message}`.
    pub fn as_defect(&self) -> Defect {
        Defect::error(self.tag(), self.message())
    }
}

impl From<VcsProcessError> for VcsError {
    fn from(error: VcsProcessError) -> Self {
        Self::Process(error)
    }
}

impl std::fmt::Display for VcsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for VcsError {}

/// `GitManagerServiceError` (`git.ts`): the members this crate produces, plus any other tagged
/// member (`GitPullRequestMaterializationError`, `SourceControlProviderError`,
/// `TextGenerationError`) passed through from WP-19/WP-20 code.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GitManagerServiceError {
    Manager(GitManagerError),
    Command(GitCommandError),
    Other(TaggedError),
}

impl GitManagerServiceError {
    pub fn message(&self) -> String {
        match self {
            Self::Manager(error) => error.message(),
            Self::Command(error) => error.message(),
            Self::Other(error) => error.to_string(),
        }
    }
}

impl From<GitManagerError> for GitManagerServiceError {
    fn from(error: GitManagerError) -> Self {
        Self::Manager(error)
    }
}

impl From<GitCommandError> for GitManagerServiceError {
    fn from(error: GitCommandError) -> Self {
        Self::Command(error)
    }
}

impl std::fmt::Display for GitManagerServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for GitManagerServiceError {}

/// `ReviewDiffPreviewError` (`review.ts`): `VcsError | GitCommandError`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ReviewDiffPreviewError {
    Vcs(VcsError),
    Git(GitCommandError),
}

impl From<VcsError> for ReviewDiffPreviewError {
    fn from(error: VcsError) -> Self {
        Self::Vcs(error)
    }
}

impl From<GitCommandError> for ReviewDiffPreviewError {
    fn from(error: GitCommandError) -> Self {
        Self::Git(error)
    }
}

impl std::fmt::Display for ReviewDiffPreviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Vcs(error) => error.fmt(f),
            Self::Git(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ReviewDiffPreviewError {}

/// Conversion into the zc-ports [`TaggedError`] placeholder.
pub trait IntoTagged {
    fn into_tagged(self) -> TaggedError;
}

fn tagged_from<T: Serialize>(value: &T, message: String) -> TaggedError {
    let encoded = serde_json::to_value(value).unwrap_or(Value::Null);
    let mut fields = match encoded {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    let tag = fields.remove("_tag").and_then(|tag| tag.as_str().map(str::to_owned)).unwrap_or_default();
    TaggedError { tag, fields, message }
}

impl IntoTagged for GitCommandError {
    fn into_tagged(self) -> TaggedError {
        let message = self.message();
        tagged_from(&self, message)
    }
}

impl IntoTagged for GitManagerError {
    fn into_tagged(self) -> TaggedError {
        let message = self.message();
        tagged_from(&self, message)
    }
}

impl IntoTagged for GitManagerServiceError {
    fn into_tagged(self) -> TaggedError {
        match self {
            Self::Manager(error) => error.into_tagged(),
            Self::Command(error) => error.into_tagged(),
            Self::Other(error) => error,
        }
    }
}

impl IntoTagged for VcsError {
    fn into_tagged(self) -> TaggedError {
        let message = self.message();
        tagged_from(&self, message)
    }
}

/// The reverse: a [`TaggedError`] received through a port, as a [`GitManagerServiceError`].
pub fn service_error_from_tagged(error: TaggedError) -> GitManagerServiceError {
    let mut value = Map::new();
    value.insert("_tag".into(), Value::String(error.tag.clone()));
    value.extend(error.fields.clone());
    match error.tag.as_str() {
        "GitManagerError" => serde_json::from_value(Value::Object(value))
            .map(GitManagerServiceError::Manager)
            .unwrap_or(GitManagerServiceError::Other(error)),
        "GitCommandError" => serde_json::from_value(Value::Object(value))
            .map(GitManagerServiceError::Command)
            .unwrap_or(GitManagerServiceError::Other(error)),
        _ => GitManagerServiceError::Other(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn git_command_error_encodes_declared_fields_only() {
        let error = GitCommandError {
            exit_code: Some(128),
            stdout_length: Some(0),
            stderr_length: Some(12),
            ..GitCommandError::git("GitVcsDriver.status", "/r", 3, "Git status failed.")
        };
        assert_eq!(
            serde_json::to_value(&error).unwrap(),
            json!({
                "_tag": "GitCommandError",
                "operation": "GitVcsDriver.status",
                "command": "git",
                "cwd": "/r",
                "argumentCount": 3,
                "exitCode": 128,
                "stdoutLength": 0,
                "stderrLength": 12,
                "detail": "Git status failed."
            })
        );
        assert_eq!(error.to_string(), "Git command failed in GitVcsDriver.status (/r): Git status failed.");
        let tagged = error.into_tagged();
        assert_eq!(tagged.tag, "GitCommandError");
        assert_eq!(tagged.fields["exitCode"], json!(128));
    }

    #[test]
    fn vcs_errors_round_trip_untagged() {
        let error = VcsError::unsupported(
            "VcsDriverRegistry.get",
            RequestedVcsKind::Kind(VcsDriverKind::Jj),
            "No jj VCS driver is registered.",
        );
        let value = serde_json::to_value(&error).unwrap();
        assert_eq!(
            value,
            json!({
                "_tag": "VcsUnsupportedOperationError",
                "operation": "VcsDriverRegistry.get",
                "kind": "jj",
                "detail": "No jj VCS driver is registered."
            })
        );
        assert_eq!(serde_json::from_value::<VcsError>(value).unwrap(), error);
        assert_eq!(
            error.as_defect().0,
            json!({
                "name": "VcsUnsupportedOperationError",
                "message": "VCS operation is unsupported for jj in VcsDriverRegistry.get: No jj VCS driver is registered."
            })
        );
    }
}
