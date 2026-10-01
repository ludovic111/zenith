//! The workspace RPC methods of `ws.ts` (lines ~3065–3170), typed on the `zc_contracts` wire
//! types, and the error folding of `projectEntriesFailureContext`,
//! `filesystemBrowseFailureContext` and `projectFileFailureContext`.
//!
//! | Method | Scope | Behaviour |
//! |---|---|---|
//! | `projects.searchEntries` | `orchestration:read` | `WorkspaceEntries.search` |
//! | `projects.searchContents` | `orchestration:read` | `WorkspaceEntries.searchContents` |
//! | `projects.listEntries` | `orchestration:read` | `WorkspaceEntries.list` |
//! | `projects.readFile` | `orchestration:read` | `WorkspaceFileSystem.readFile` |
//! | `projects.writeFile` | `orchestration:operate` | `WorkspaceFileSystem.writeFile` |
//! | `filesystem.browse` | `orchestration:read` | `WorkspaceEntries.browse` |
//! | `shell.openInEditor` | `orchestration:operate` | `ExternalLauncher.launchEditor` |
//!
//! Payloads are checked like the TS schemas decode them (trimmed strings, non-empty, maximum
//! lengths, integer limits); a payload that fails is a per-request defect (`Die`) with the
//! reason, like a schema decode failure. Every internal failure becomes the method's tagged
//! error with the structured context and the internal error as `cause`.

use std::sync::Arc;

use zc_contracts::{
    FilesystemBrowseError, FilesystemBrowseFailure, FilesystemBrowseInput, FilesystemBrowseResult, LaunchEditorInput, LitFilesystemBrowseError,
    LitProjectListEntriesError, LitProjectReadFileError, LitProjectSearchContentsError, LitProjectSearchEntriesError, LitProjectWriteFileError,
    ProjectEntriesFailure, ProjectFileFailure, ProjectFileOperation, ProjectListEntriesError, ProjectListEntriesInput, ProjectListEntriesResult,
    ProjectReadFileError, ProjectReadFileInput, ProjectReadFileResult, ProjectSearchContentsError, ProjectSearchContentsInput, ProjectSearchContentsResult,
    ProjectSearchEntriesError, ProjectSearchEntriesInput, ProjectSearchEntriesResult, ProjectWriteFileError, ProjectWriteFileInput, ProjectWriteFileResult,
};
use zc_rpc::{Failure, MethodOptions, RpcMethod, RpcRouterBuilder, ScopeRule};

use crate::entries::{BrowseRequest, EntrySearch, WorkspaceEntries};
use crate::errors::{BrowseError, EntriesError, FileOperation, FileSystemError, SearchIndexError, TaggedError, WorkspaceRootError};
use crate::file_system::WorkspaceFileSystem;
use crate::launcher::{ExternalLauncher, ExternalLauncherError};
use crate::search_index::ContentSearch;
use crate::text::js_length;

pub const ORCHESTRATION_READ: &str = "orchestration:read";
pub const ORCHESTRATION_OPERATE: &str = "orchestration:operate";

macro_rules! method {
    ($name:ident, $tag:literal, $payload:ty, $success:ty, $error:ty) => {
        #[doc = concat!("`", $tag, "`.")]
        pub struct $name;
        impl RpcMethod for $name {
            const TAG: &'static str = $tag;
            const STREAM: bool = false;
            type Payload = $payload;
            type Success = $success;
            type Error = $error;
        }
    };
}

method!(
    ProjectsSearchEntries,
    "projects.searchEntries",
    ProjectSearchEntriesInput,
    ProjectSearchEntriesResult,
    ProjectSearchEntriesError
);
method!(
    ProjectsSearchContents,
    "projects.searchContents",
    ProjectSearchContentsInput,
    ProjectSearchContentsResult,
    ProjectSearchContentsError
);
method!(
    ProjectsListEntries,
    "projects.listEntries",
    ProjectListEntriesInput,
    ProjectListEntriesResult,
    ProjectListEntriesError
);
method!(
    ProjectsReadFile,
    "projects.readFile",
    ProjectReadFileInput,
    ProjectReadFileResult,
    ProjectReadFileError
);
method!(
    ProjectsWriteFile,
    "projects.writeFile",
    ProjectWriteFileInput,
    ProjectWriteFileResult,
    ProjectWriteFileError
);
method!(
    FilesystemBrowse,
    "filesystem.browse",
    FilesystemBrowseInput,
    FilesystemBrowseResult,
    FilesystemBrowseError
);
method!(ShellOpenInEditor, "shell.openInEditor", LaunchEditorInput, (), ExternalLauncherError);

/// Every method this module registers, with its scope (`RPC_REQUIRED_SCOPES`).
pub const METHOD_SCOPES: [(&str, &str); 7] = [
    ("projects.searchEntries", ORCHESTRATION_READ),
    ("projects.searchContents", ORCHESTRATION_READ),
    ("projects.listEntries", ORCHESTRATION_READ),
    ("projects.readFile", ORCHESTRATION_READ),
    ("projects.writeFile", ORCHESTRATION_OPERATE),
    ("filesystem.browse", ORCHESTRATION_READ),
    ("shell.openInEditor", ORCHESTRATION_OPERATE),
];

/// What the handlers need.
#[derive(Clone)]
pub struct WorkspaceRpcServices {
    pub entries: WorkspaceEntries,
    pub file_system: WorkspaceFileSystem,
    pub launcher: Arc<ExternalLauncher>,
}

// ---------------------------------------------------------------------------------------------
// Payload decoding (the schema checks serde does not do)
// ---------------------------------------------------------------------------------------------

/// A payload that does not satisfy its schema: a per-request defect whose value is the reason
/// text (what zc-rpc does for payloads serde cannot decode, like the TS decode failure).
fn invalid<E>(reason: impl std::fmt::Display) -> Failure<E> {
    Failure::Die(serde_json::Value::String(reason.to_string()))
}

fn trimmed_non_empty<E>(field: &str, value: &str, max: Option<usize>) -> Result<String, Failure<E>> {
    let value = value.trim();
    if value.is_empty() {
        return Err(invalid(format!("Expected a non-empty value at [\"{field}\"]")));
    }
    if let Some(max) = max {
        if js_length(value) > max {
            return Err(invalid(format!("Expected a value with a length of at most {max} at [\"{field}\"]")));
        }
    }
    Ok(value.to_owned())
}

fn limit<E>(value: i64, max: i64) -> Result<usize, Failure<E>> {
    if value < 1 || value > max {
        return Err(invalid(format!("Expected an integer between 1 and {max} at [\"limit\"], got {value}")));
    }
    Ok(value as usize)
}

/// Trim, then drop empties (`TrimmedNonEmptyString` encode side of optional error fields).
fn opt(value: Option<&str>) -> Option<String> {
    value.map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned)
}

// ---------------------------------------------------------------------------------------------
// Error folding (ws.ts)
// ---------------------------------------------------------------------------------------------

/// `projectEntriesFailureContext`: `(failure, normalizedCwd, timeout, detail)`.
fn entries_failure_context(error: &EntriesError) -> (ProjectEntriesFailure, Option<String>, Option<String>, Option<String>) {
    match error {
        EntriesError::Root(root) => {
            let normalized = Some(root.normalized_workspace_root().to_owned());
            match root {
                WorkspaceRootError::NotExists { .. } => (ProjectEntriesFailure::WorkspaceRootNotFound, normalized, None, None),
                WorkspaceRootError::CreateFailed { .. } => (ProjectEntriesFailure::WorkspaceRootCreateFailed, normalized, None, None),
                WorkspaceRootError::StatFailed { phase, .. } => (
                    ProjectEntriesFailure::WorkspaceRootStatFailed,
                    normalized,
                    None,
                    Some(phase.as_str().to_owned()),
                ),
                WorkspaceRootError::NotDirectory { .. } => (ProjectEntriesFailure::WorkspaceRootNotDirectory, normalized, None, None),
            }
        }
        EntriesError::ReadDirectory(read) => (ProjectEntriesFailure::DirectoryListFailed, read.cwd.clone(), None, Some(read.message())),
        EntriesError::Index(index) => match index {
            SearchIndexError::CreateFailed { cwd, reason, .. } => {
                (ProjectEntriesFailure::SearchIndexCreateFailed, Some(cwd.clone()), None, Some(reason.clone()))
            }
            SearchIndexError::ScanTimedOut { cwd, timeout } => (ProjectEntriesFailure::SearchIndexScanTimedOut, Some(cwd.clone()), Some(timeout.clone()), None),
            SearchIndexError::SearchFailed { cwd, reason, .. } | SearchIndexError::RefreshFailed { cwd, reason, .. } => {
                (ProjectEntriesFailure::SearchIndexSearchFailed, Some(cwd.clone()), None, Some(reason.clone()))
            }
        },
    }
}

fn file_operation(operation: FileOperation) -> ProjectFileOperation {
    match operation {
        FileOperation::RealpathWorkspaceRoot => ProjectFileOperation::RealpathWorkspaceRoot,
        FileOperation::RealpathTarget => ProjectFileOperation::RealpathTarget,
        FileOperation::Open => ProjectFileOperation::Open,
        FileOperation::Stat => ProjectFileOperation::Stat,
        FileOperation::Read => ProjectFileOperation::Read,
        FileOperation::Close => ProjectFileOperation::Close,
        FileOperation::MakeDirectory => ProjectFileOperation::MakeDirectory,
        FileOperation::WriteFile => ProjectFileOperation::WriteFile,
    }
}

/// `projectFileFailureContext`.
struct FileFailureContext {
    failure: ProjectFileFailure,
    resolved_path: Option<String>,
    resolved_workspace_root: Option<String>,
    operation: Option<ProjectFileOperation>,
    operation_path: Option<String>,
}

fn file_failure_context(error: &FileSystemError) -> FileFailureContext {
    let base = |failure| FileFailureContext {
        failure,
        resolved_path: None,
        resolved_workspace_root: None,
        operation: None,
        operation_path: None,
    };
    match error {
        FileSystemError::OutsideRoot(_) => base(ProjectFileFailure::WorkspacePathOutsideRoot),
        FileSystemError::Operation {
            resolved_path,
            operation,
            operation_path,
            ..
        } => FileFailureContext {
            resolved_path: Some(resolved_path.clone()),
            operation: Some(file_operation(*operation)),
            operation_path: Some(operation_path.clone()),
            ..base(ProjectFileFailure::OperationFailed)
        },
        FileSystemError::PathEscape {
            resolved_path,
            resolved_workspace_root,
            ..
        } => FileFailureContext {
            resolved_path: Some(resolved_path.clone()),
            resolved_workspace_root: Some(resolved_workspace_root.clone()),
            ..base(ProjectFileFailure::ResolvedPathOutsideRoot)
        },
        FileSystemError::NotFile { resolved_path, .. } => FileFailureContext {
            resolved_path: Some(resolved_path.clone()),
            ..base(ProjectFileFailure::PathNotFile)
        },
        FileSystemError::Binary { resolved_path, .. } => FileFailureContext {
            resolved_path: Some(resolved_path.clone()),
            ..base(ProjectFileFailure::BinaryFile)
        },
    }
}

/// `new ProjectSearchEntriesError({ cwd, queryLength, limit, ...context, cause })`.
pub fn search_entries_error(cwd: &str, query_length: usize, limit: i64, error: &EntriesError) -> ProjectSearchEntriesError {
    let (failure, normalized_cwd, timeout, detail) = entries_failure_context(error);
    ProjectSearchEntriesError {
        tag: LitProjectSearchEntriesError,
        cwd: opt(Some(cwd)),
        query_length: Some(query_length as i64),
        limit: Some(limit),
        failure: Some(failure),
        normalized_cwd: opt(normalized_cwd.as_deref()),
        timeout: opt(timeout.as_deref()),
        detail: opt(detail.as_deref()),
        message: format!("Failed to search workspace entries in '{cwd}'."),
        cause: Some(error.to_defect().0),
    }
}

/// `new ProjectSearchContentsError({ cwd, queryLength, limit, ...context, cause })`.
pub fn search_contents_error(cwd: &str, query_length: usize, limit: i64, error: &EntriesError) -> ProjectSearchContentsError {
    let (failure, normalized_cwd, timeout, detail) = entries_failure_context(error);
    ProjectSearchContentsError {
        tag: LitProjectSearchContentsError,
        cwd: opt(Some(cwd)),
        query_length: Some(query_length as i64),
        limit: Some(limit),
        failure: Some(failure),
        normalized_cwd: opt(normalized_cwd.as_deref()),
        timeout: opt(timeout.as_deref()),
        detail: opt(detail.as_deref()),
        message: format!("Failed to search workspace contents in '{cwd}'."),
        cause: Some(error.to_defect().0),
    }
}

/// `new ProjectListEntriesError({ ...input, ...context, cause })`.
pub fn list_entries_error(cwd: &str, error: &EntriesError) -> ProjectListEntriesError {
    let (failure, normalized_cwd, timeout, detail) = entries_failure_context(error);
    ProjectListEntriesError {
        tag: LitProjectListEntriesError,
        cwd: opt(Some(cwd)),
        failure: Some(failure),
        normalized_cwd: opt(normalized_cwd.as_deref()),
        timeout: opt(timeout.as_deref()),
        detail: opt(detail.as_deref()),
        message: format!("Failed to list workspace entries in '{cwd}'."),
        cause: Some(error.to_defect().0),
    }
}

/// `new ProjectReadFileError({ ...input, ...context, cause })`.
pub fn read_file_error(cwd: &str, relative_path: &str, error: &FileSystemError) -> ProjectReadFileError {
    let context = file_failure_context(error);
    ProjectReadFileError {
        tag: LitProjectReadFileError,
        cwd: opt(Some(cwd)),
        relative_path: opt(Some(relative_path)),
        failure: Some(context.failure),
        resolved_path: opt(context.resolved_path.as_deref()),
        resolved_workspace_root: opt(context.resolved_workspace_root.as_deref()),
        operation: context.operation,
        operation_path: opt(context.operation_path.as_deref()),
        message: format!("Failed to read workspace file '{relative_path}' in '{cwd}'."),
        cause: Some(error.to_defect().0),
    }
}

/// `new ProjectWriteFileError({ cwd, relativePath, ...context, cause })`.
pub fn write_file_error(cwd: &str, relative_path: &str, error: &FileSystemError) -> ProjectWriteFileError {
    let context = file_failure_context(error);
    ProjectWriteFileError {
        tag: LitProjectWriteFileError,
        cwd: opt(Some(cwd)),
        relative_path: opt(Some(relative_path)),
        failure: Some(context.failure),
        resolved_path: opt(context.resolved_path.as_deref()),
        resolved_workspace_root: opt(context.resolved_workspace_root.as_deref()),
        operation: context.operation,
        operation_path: opt(context.operation_path.as_deref()),
        message: format!("Failed to write workspace file '{relative_path}' in '{cwd}'."),
        cause: Some(error.to_defect().0),
    }
}

/// `new FilesystemBrowseError({ ...input, ...context, cause })`.
pub fn browse_error(input: &BrowseRequest, error: &BrowseError) -> FilesystemBrowseError {
    let (failure, parent_path, platform) = match error {
        BrowseError::WindowsPathUnsupported { platform, .. } => (FilesystemBrowseFailure::WindowsPathUnsupported, None, Some(platform.clone())),
        BrowseError::CurrentProjectRequired { .. } => (FilesystemBrowseFailure::CurrentProjectRequired, None, None),
        BrowseError::ReadDirectory(read) => (FilesystemBrowseFailure::ReadDirectoryFailed, Some(read.parent_path.clone()), None),
    };
    let from = input.cwd.as_deref().map(|cwd| format!(" from '{cwd}'")).unwrap_or_default();
    FilesystemBrowseError {
        tag: LitFilesystemBrowseError,
        partial_path: opt(Some(&input.partial_path)),
        cwd: opt(input.cwd.as_deref()),
        failure: Some(failure),
        parent_path: opt(parent_path.as_deref()),
        platform: opt(platform.as_deref()),
        message: format!("Failed to browse filesystem path '{}'{from}.", input.partial_path),
        cause: Some(error.to_defect().0),
    }
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `projects.searchEntries`.
pub async fn search_entries(
    services: &WorkspaceRpcServices,
    input: ProjectSearchEntriesInput,
) -> Result<ProjectSearchEntriesResult, Failure<ProjectSearchEntriesError>> {
    let cwd = trimmed_non_empty("cwd", &input.cwd, None)?;
    let query = input.query.trim().to_owned();
    if js_length(&query) > 256 {
        return Err(invalid("Expected a value with a length of at most 256 at [\"query\"]"));
    }
    let limit_value = limit(input.limit, 200)?;
    let search = EntrySearch {
        query: query.clone(),
        limit: limit_value,
        kind: input.kind,
        image_only: input.image_only.unwrap_or(false),
    };
    services
        .entries
        .search(&cwd, &search)
        .await
        .map_err(|error| Failure::Fail(search_entries_error(&cwd, js_length(&query), input.limit, &error)))
}

/// `projects.searchContents`.
pub async fn search_contents(
    services: &WorkspaceRpcServices,
    input: ProjectSearchContentsInput,
) -> Result<ProjectSearchContentsResult, Failure<ProjectSearchContentsError>> {
    let cwd = trimmed_non_empty("cwd", &input.cwd, None)?;
    if input.query.is_empty() {
        return Err(invalid("Expected a non-empty value at [\"query\"]"));
    }
    if js_length(&input.query) > 256 {
        return Err(invalid("Expected a value with a length of at most 256 at [\"query\"]"));
    }
    let limit_value = limit(input.limit, 500)?;
    let search = ContentSearch {
        query: input.query.clone(),
        limit: limit_value,
        case_sensitive: input.case_sensitive,
        whole_word: input.whole_word,
        use_regex: input.use_regex,
    };
    services
        .entries
        .search_contents(&cwd, &search)
        .await
        .map_err(|error| Failure::Fail(search_contents_error(&cwd, js_length(&input.query), input.limit, &error)))
}

/// `projects.listEntries`.
pub async fn list_entries(
    services: &WorkspaceRpcServices,
    input: ProjectListEntriesInput,
) -> Result<ProjectListEntriesResult, Failure<ProjectListEntriesError>> {
    let cwd = trimmed_non_empty("cwd", &input.cwd, None)?;
    let directory_path = input.directory_path.as_deref().map(str::trim);
    services
        .entries
        .list(&cwd, directory_path)
        .await
        .map_err(|error| Failure::Fail(list_entries_error(&cwd, &error)))
}

/// `projects.readFile`.
pub async fn read_file(services: &WorkspaceRpcServices, input: ProjectReadFileInput) -> Result<ProjectReadFileResult, Failure<ProjectReadFileError>> {
    let cwd = trimmed_non_empty("cwd", &input.cwd, None)?;
    let relative_path = trimmed_non_empty("relativePath", &input.relative_path, Some(512))?;
    services
        .file_system
        .read_file(&cwd, &relative_path)
        .await
        .map_err(|error| Failure::Fail(read_file_error(&cwd, &relative_path, &error)))
}

/// `projects.writeFile`.
pub async fn write_file(services: &WorkspaceRpcServices, input: ProjectWriteFileInput) -> Result<ProjectWriteFileResult, Failure<ProjectWriteFileError>> {
    let cwd = trimmed_non_empty("cwd", &input.cwd, None)?;
    let relative_path = trimmed_non_empty("relativePath", &input.relative_path, Some(512))?;
    services
        .file_system
        .write_file(&cwd, &relative_path, &input.contents)
        .await
        .map_err(|error| Failure::Fail(write_file_error(&cwd, &relative_path, &error)))
}

/// `filesystem.browse`.
pub async fn browse(services: &WorkspaceRpcServices, input: FilesystemBrowseInput) -> Result<FilesystemBrowseResult, Failure<FilesystemBrowseError>> {
    let partial_path = trimmed_non_empty("partialPath", &input.partial_path, Some(512))?;
    let cwd = match input.cwd.as_deref() {
        Some(cwd) => Some(trimmed_non_empty("cwd", cwd, Some(512))?),
        None => None,
    };
    let request = BrowseRequest { partial_path, cwd };
    services
        .entries
        .browse(&request)
        .await
        .map_err(|error| Failure::Fail(browse_error(&request, &error)))
}

/// `shell.openInEditor`.
pub async fn open_in_editor(services: &WorkspaceRpcServices, input: LaunchEditorInput) -> Result<(), Failure<ExternalLauncherError>> {
    let cwd = trimmed_non_empty("cwd", &input.cwd, None)?;
    services
        .launcher
        .launch_editor(&LaunchEditorInput { cwd, ..input })
        .await
        .map_err(Failure::Fail)
}

fn scope(scope: &'static str) -> MethodOptions {
    MethodOptions::default().scope(ScopeRule::required(scope))
}

macro_rules! register_unary {
    ($builder:expr, $services:expr, $method:ty, $scope:expr, $handler:path) => {{
        let services = $services.clone();
        $builder.typed_unary_with::<$method, _, _>(scope($scope), move |_ctx, input| {
            let services = services.clone();
            async move { $handler(&services, input).await }
        })
    }};
}

/// Registers the seven methods.
pub fn register(builder: RpcRouterBuilder, services: WorkspaceRpcServices) -> RpcRouterBuilder {
    let builder = register_unary!(builder, services, ProjectsSearchEntries, ORCHESTRATION_READ, search_entries);
    let builder = register_unary!(builder, services, ProjectsSearchContents, ORCHESTRATION_READ, search_contents);
    let builder = register_unary!(builder, services, ProjectsListEntries, ORCHESTRATION_READ, list_entries);
    let builder = register_unary!(builder, services, ProjectsReadFile, ORCHESTRATION_READ, read_file);
    let builder = register_unary!(builder, services, ProjectsWriteFile, ORCHESTRATION_OPERATE, write_file);
    let builder = register_unary!(builder, services, FilesystemBrowse, ORCHESTRATION_READ, browse);
    register_unary!(builder, services, ShellOpenInEditor, ORCHESTRATION_OPERATE, open_in_editor)
}
