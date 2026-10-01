//! The internal tagged errors of `workspace/**`, field for field, with the TS `message` getters.
//!
//! They never cross the wire as they are: the RPC layer ([`crate::rpc`]) folds them into the
//! contract errors (`ProjectSearchEntriesError`, …) and keeps the internal error as the
//! `cause` defect, encoded the way Effect encodes an `Error` instance:
//! `{"name": <_tag>, "message": <message>, "cause"?: <inner defect>}`.

use serde_json::{json, Map, Value};
use zc_core::Defect;

/// A tagged error: its `_tag`, its message, and its own `cause` (a defect) when it has one.
pub trait TaggedError: std::fmt::Debug {
    fn tag(&self) -> &'static str;
    fn message(&self) -> String;
    fn cause(&self) -> Option<&Defect> {
        None
    }
    /// The error as a `Schema.Defect()` value.
    fn to_defect(&self) -> Defect {
        let mut map = Map::new();
        map.insert("name".into(), Value::String(self.tag().into()));
        map.insert("message".into(), Value::String(self.message()));
        if let Some(cause) = self.cause() {
            map.insert("cause".into(), cause.0.clone());
        }
        Defect(Value::Object(map))
    }
}

macro_rules! display_via_message {
    ($($ty:ty),* $(,)?) => {$(
        impl std::fmt::Display for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&TaggedError::message(self))
            }
        }
        impl std::error::Error for $ty {}
    )*};
}

/// `" from '<cwd>'"` or nothing, the suffix the browse errors share.
fn from_cwd(cwd: Option<&str>) -> String {
    cwd.filter(|cwd| !cwd.is_empty()).map(|cwd| format!(" from '{cwd}'")).unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// WorkspacePaths
// ---------------------------------------------------------------------------------------------

/// `WorkspaceRootStatFailedError.phase`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatPhase {
    ValidateExisting,
    VerifyCreated,
}

impl StatPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ValidateExisting => "validate-existing",
            Self::VerifyCreated => "verify-created",
        }
    }
}

/// The failures of `normalizeWorkspaceRoot`.
#[derive(Debug, Clone, PartialEq)]
pub enum WorkspaceRootError {
    NotExists {
        workspace_root: String,
        normalized_workspace_root: String,
    },
    CreateFailed {
        workspace_root: String,
        normalized_workspace_root: String,
        cause: Defect,
    },
    StatFailed {
        workspace_root: String,
        normalized_workspace_root: String,
        phase: StatPhase,
        cause: Defect,
    },
    NotDirectory {
        workspace_root: String,
        normalized_workspace_root: String,
    },
}

impl WorkspaceRootError {
    pub fn normalized_workspace_root(&self) -> &str {
        match self {
            Self::NotExists { normalized_workspace_root, .. }
            | Self::CreateFailed { normalized_workspace_root, .. }
            | Self::StatFailed { normalized_workspace_root, .. }
            | Self::NotDirectory { normalized_workspace_root, .. } => normalized_workspace_root,
        }
    }
}

impl TaggedError for WorkspaceRootError {
    fn tag(&self) -> &'static str {
        match self {
            Self::NotExists { .. } => "WorkspaceRootNotExistsError",
            Self::CreateFailed { .. } => "WorkspaceRootCreateFailedError",
            Self::StatFailed { .. } => "WorkspaceRootStatFailedError",
            Self::NotDirectory { .. } => "WorkspaceRootNotDirectoryError",
        }
    }
    fn message(&self) -> String {
        match self {
            Self::NotExists { normalized_workspace_root, .. } => {
                format!("Workspace root does not exist: {normalized_workspace_root}")
            }
            Self::CreateFailed { normalized_workspace_root, .. } => {
                format!("Failed to create workspace root: {normalized_workspace_root}")
            }
            Self::StatFailed {
                normalized_workspace_root,
                phase,
                ..
            } => format!("Failed to stat workspace root '{normalized_workspace_root}' during '{}'.", phase.as_str()),
            Self::NotDirectory { normalized_workspace_root, .. } => {
                format!("Workspace root is not a directory: {normalized_workspace_root}")
            }
        }
    }
    fn cause(&self) -> Option<&Defect> {
        match self {
            Self::CreateFailed { cause, .. } | Self::StatFailed { cause, .. } => Some(cause),
            _ => None,
        }
    }
}

/// `WorkspacePathOutsideRootError`: an absolute or escaping workspace-relative path.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspacePathOutsideRootError {
    pub workspace_root: String,
    pub relative_path: String,
}

impl TaggedError for WorkspacePathOutsideRootError {
    fn tag(&self) -> &'static str {
        "WorkspacePathOutsideRootError"
    }
    fn message(&self) -> String {
        format!("Workspace file path must be relative to the project root: {}", self.relative_path)
    }
}

// ---------------------------------------------------------------------------------------------
// WorkspaceSearchIndex
// ---------------------------------------------------------------------------------------------

/// The failures of the search index (`WorkspaceSearchIndex*` tagged errors).
#[derive(Debug, Clone, PartialEq)]
pub enum SearchIndexError {
    CreateFailed {
        cwd: String,
        reason: String,
        cause: Option<Defect>,
    },
    ScanTimedOut {
        cwd: String,
        timeout: String,
    },
    SearchFailed {
        cwd: String,
        query_length: usize,
        page_size: usize,
        reason: String,
        cause: Option<Defect>,
    },
    RefreshFailed {
        cwd: String,
        reason: String,
        cause: Option<Defect>,
    },
}

impl TaggedError for SearchIndexError {
    fn tag(&self) -> &'static str {
        match self {
            Self::CreateFailed { .. } => "WorkspaceSearchIndexCreateFailed",
            Self::ScanTimedOut { .. } => "WorkspaceSearchIndexScanTimedOut",
            Self::SearchFailed { .. } => "WorkspaceSearchIndexSearchFailed",
            Self::RefreshFailed { .. } => "WorkspaceSearchIndexRefreshFailed",
        }
    }
    fn message(&self) -> String {
        match self {
            Self::CreateFailed { cwd, .. } => {
                format!("Failed to create the workspace search index for '{cwd}'.")
            }
            Self::ScanTimedOut { cwd, timeout } => {
                format!("Workspace search index for '{cwd}' did not finish scanning within {timeout}")
            }
            Self::SearchFailed { cwd, .. } => format!("Workspace search failed for '{cwd}'."),
            Self::RefreshFailed { cwd, .. } => {
                format!("Failed to refresh the workspace search index for '{cwd}'.")
            }
        }
    }
    fn cause(&self) -> Option<&Defect> {
        match self {
            Self::CreateFailed { cause, .. } | Self::SearchFailed { cause, .. } | Self::RefreshFailed { cause, .. } => cause.as_ref(),
            Self::ScanTimedOut { .. } => None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// WorkspaceEntries
// ---------------------------------------------------------------------------------------------

/// `WorkspaceEntriesReadDirectoryError`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadDirectoryError {
    pub cwd: Option<String>,
    pub partial_path: String,
    pub parent_path: String,
    pub cause: Defect,
}

impl TaggedError for ReadDirectoryError {
    fn tag(&self) -> &'static str {
        "WorkspaceEntriesReadDirectoryError"
    }
    fn message(&self) -> String {
        format!(
            "Failed to read workspace directory '{}' while browsing '{}'{}.",
            self.parent_path,
            self.partial_path,
            from_cwd(self.cwd.as_deref())
        )
    }
    fn cause(&self) -> Option<&Defect> {
        Some(&self.cause)
    }
}

/// `WorkspaceEntriesBrowseError`.
#[derive(Debug, Clone, PartialEq)]
pub enum BrowseError {
    WindowsPathUnsupported {
        cwd: Option<String>,
        partial_path: String,
        platform: String,
    },
    CurrentProjectRequired {
        partial_path: String,
    },
    ReadDirectory(ReadDirectoryError),
}

impl TaggedError for BrowseError {
    fn tag(&self) -> &'static str {
        match self {
            Self::WindowsPathUnsupported { .. } => "WorkspaceEntriesWindowsPathUnsupportedError",
            Self::CurrentProjectRequired { .. } => "WorkspaceEntriesCurrentProjectRequiredError",
            Self::ReadDirectory(error) => error.tag(),
        }
    }
    fn message(&self) -> String {
        match self {
            Self::WindowsPathUnsupported { cwd, partial_path, platform } => format!(
                "Windows-style workspace path '{partial_path}' is not supported on '{platform}'{}.",
                from_cwd(cwd.as_deref())
            ),
            Self::CurrentProjectRequired { partial_path } => format!("A current project is required to browse relative workspace path '{partial_path}'."),
            Self::ReadDirectory(error) => error.message(),
        }
    }
    fn cause(&self) -> Option<&Defect> {
        match self {
            Self::ReadDirectory(error) => error.cause(),
            _ => None,
        }
    }
}

/// `WorkspaceEntriesError`.
#[derive(Debug, Clone, PartialEq)]
pub enum EntriesError {
    ReadDirectory(ReadDirectoryError),
    Root(WorkspaceRootError),
    Index(SearchIndexError),
}

impl From<WorkspaceRootError> for EntriesError {
    fn from(error: WorkspaceRootError) -> Self {
        Self::Root(error)
    }
}

impl From<SearchIndexError> for EntriesError {
    fn from(error: SearchIndexError) -> Self {
        Self::Index(error)
    }
}

impl TaggedError for EntriesError {
    fn tag(&self) -> &'static str {
        match self {
            Self::ReadDirectory(error) => error.tag(),
            Self::Root(error) => error.tag(),
            Self::Index(error) => error.tag(),
        }
    }
    fn message(&self) -> String {
        match self {
            Self::ReadDirectory(error) => error.message(),
            Self::Root(error) => error.message(),
            Self::Index(error) => error.message(),
        }
    }
    fn cause(&self) -> Option<&Defect> {
        match self {
            Self::ReadDirectory(error) => error.cause(),
            Self::Root(error) => error.cause(),
            Self::Index(error) => error.cause(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// WorkspaceFileSystem
// ---------------------------------------------------------------------------------------------

/// `WorkspaceFileSystemOperationError.operation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOperation {
    RealpathWorkspaceRoot,
    RealpathTarget,
    Open,
    Stat,
    Read,
    Close,
    MakeDirectory,
    WriteFile,
}

impl FileOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RealpathWorkspaceRoot => "realpath-workspace-root",
            Self::RealpathTarget => "realpath-target",
            Self::Open => "open",
            Self::Stat => "stat",
            Self::Read => "read",
            Self::Close => "close",
            Self::MakeDirectory => "make-directory",
            Self::WriteFile => "write-file",
        }
    }
}

/// `WorkspaceFileSystemError | WorkspacePathOutsideRootError`.
#[derive(Debug, Clone, PartialEq)]
pub enum FileSystemError {
    Operation {
        workspace_root: String,
        relative_path: String,
        resolved_path: String,
        operation_path: String,
        operation: FileOperation,
        cause: Defect,
    },
    PathEscape {
        workspace_root: String,
        relative_path: String,
        resolved_workspace_root: String,
        resolved_path: String,
    },
    NotFile {
        workspace_root: String,
        relative_path: String,
        resolved_path: String,
    },
    Binary {
        workspace_root: String,
        relative_path: String,
        resolved_path: String,
    },
    OutsideRoot(WorkspacePathOutsideRootError),
}

impl From<WorkspacePathOutsideRootError> for FileSystemError {
    fn from(error: WorkspacePathOutsideRootError) -> Self {
        Self::OutsideRoot(error)
    }
}

impl TaggedError for FileSystemError {
    fn tag(&self) -> &'static str {
        match self {
            Self::Operation { .. } => "WorkspaceFileSystemOperationError",
            Self::PathEscape { .. } => "WorkspaceFilePathEscapeError",
            Self::NotFile { .. } => "WorkspacePathNotFileError",
            Self::Binary { .. } => "WorkspaceBinaryFileError",
            Self::OutsideRoot(error) => error.tag(),
        }
    }
    fn message(&self) -> String {
        match self {
            Self::Operation { workspace_root, relative_path, resolved_path, operation_path, operation, .. } => format!(
                "Workspace file operation '{}' failed at '{operation_path}' for resolved path '{resolved_path}' (requested as '{relative_path}' in '{workspace_root}').",
                operation.as_str()
            ),
            Self::PathEscape { workspace_root, relative_path, resolved_path, .. } => format!(
                "Workspace file '{relative_path}' resolves outside workspace root '{workspace_root}': {resolved_path}"
            ),
            Self::NotFile { workspace_root, relative_path, resolved_path } => format!(
                "Workspace path '{relative_path}' in '{workspace_root}' is not a file: {resolved_path}"
            ),
            Self::Binary { workspace_root, relative_path, .. } => format!(
                "Workspace file '{relative_path}' in '{workspace_root}' is binary and cannot be previewed as text."
            ),
            Self::OutsideRoot(error) => error.message(),
        }
    }
    fn cause(&self) -> Option<&Defect> {
        match self {
            Self::Operation { cause, .. } => Some(cause),
            _ => None,
        }
    }
}

display_via_message!(
    WorkspaceRootError,
    WorkspacePathOutsideRootError,
    SearchIndexError,
    ReadDirectoryError,
    BrowseError,
    EntriesError,
    FileSystemError,
);

// ---------------------------------------------------------------------------------------------
// Node-style I/O causes
// ---------------------------------------------------------------------------------------------

/// The `code` and libuv description of an `io::Error`, as Node reports them (`ENOENT`, "no such
/// file or directory").
pub fn node_error_code(error: &std::io::Error) -> (&'static str, &'static str) {
    match error.raw_os_error() {
        Some(libc::ENOENT) => ("ENOENT", "no such file or directory"),
        Some(libc::EACCES) => ("EACCES", "permission denied"),
        Some(libc::EPERM) => ("EPERM", "operation not permitted"),
        Some(libc::ENOTDIR) => ("ENOTDIR", "not a directory"),
        Some(libc::EISDIR) => ("EISDIR", "illegal operation on a directory"),
        Some(libc::ELOOP) => ("ELOOP", "too many symbolic links encountered"),
        Some(libc::ENAMETOOLONG) => ("ENAMETOOLONG", "name too long"),
        Some(libc::EEXIST) => ("EEXIST", "file already exists"),
        Some(libc::EMFILE) => ("EMFILE", "too many open files"),
        Some(libc::ENOSPC) => ("ENOSPC", "no space left on device"),
        Some(libc::EROFS) => ("EROFS", "read-only file system"),
        Some(libc::EBUSY) => ("EBUSY", "resource busy or locked"),
        Some(libc::EINVAL) => ("EINVAL", "invalid argument"),
        Some(libc::EAGAIN) => ("EAGAIN", "resource temporarily unavailable"),
        Some(libc::EIO) => ("EIO", "i/o error"),
        _ => ("UNKNOWN", "unknown error"),
    }
}

/// A Node `fs` error as a defect: `{"name":"Error","message":"ENOENT: no such file or directory,
/// realpath '/x'"}` (Effect keeps only `name` and `message` of an `Error`).
pub fn node_fs_defect(error: &std::io::Error, syscall: &str, path: &str) -> Defect {
    let (code, description) = node_error_code(error);
    let message = if code == "UNKNOWN" {
        format!("{error}, {syscall} '{path}'")
    } else {
        format!("{code}: {description}, {syscall} '{path}'")
    };
    Defect(json!({ "name": "Error", "message": message }))
}

/// An Effect `PlatformError` (`FileSystem.makeDirectory`, `writeFileString`) as a defect.
pub fn platform_error_defect(error: &std::io::Error, method: &str, syscall: &str, path: &str) -> Defect {
    let reason = match error.kind() {
        std::io::ErrorKind::NotFound => "NotFound",
        std::io::ErrorKind::PermissionDenied => "PermissionDenied",
        std::io::ErrorKind::AlreadyExists => "AlreadyExists",
        _ => match error.raw_os_error() {
            Some(libc::ENOTDIR) | Some(libc::EISDIR) => "BadResource",
            _ => "Unknown",
        },
    };
    let inner = node_fs_defect(error, syscall, path);
    Defect(json!({
        "name": "PlatformError",
        "message": format!("{reason}: FileSystem.{method} ({path}): {}", inner.message()),
        "cause": inner.0,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defects_nest_causes_like_effect() {
        let error = SearchIndexError::SearchFailed {
            cwd: "/workspace/project".into(),
            query_length: 3,
            page_size: 4,
            reason: "FileFinder.mixedSearch threw unexpectedly.".into(),
            cause: Some(Defect::error("Error", "native search failed")),
        };
        assert_eq!(
            serde_json::to_value(error.to_defect()).unwrap(),
            json!({
                "name": "WorkspaceSearchIndexSearchFailed",
                "message": "Workspace search failed for '/workspace/project'.",
                "cause": { "name": "Error", "message": "native search failed" },
            })
        );
        let timeout = SearchIndexError::ScanTimedOut {
            cwd: "/w".into(),
            timeout: "15 seconds".into(),
        };
        assert_eq!(
            timeout.to_defect().0,
            json!({
                "name": "WorkspaceSearchIndexScanTimedOut",
                "message": "Workspace search index for '/w' did not finish scanning within 15 seconds",
            })
        );
    }

    #[test]
    fn node_style_messages() {
        let error = std::io::Error::from_raw_os_error(libc::ENOENT);
        assert_eq!(
            node_fs_defect(&error, "realpath", "/w/missing.txt").message(),
            "ENOENT: no such file or directory, realpath '/w/missing.txt'"
        );
    }
}
