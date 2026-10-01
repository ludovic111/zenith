//! `project/T3ProjectFileLoader.ts`: the checked-in `t3.json` of a workspace root.
//!
//! Best effort: a missing file is `None`; an unreadable, malformed or schema-invalid file is
//! logged and also `None`, so callers fall back to their defaults. The file is JSONC (comments
//! and trailing commas), validated whole like `T3ProjectFileFromJson` (zc-vcs's
//! `parse_t3_project_file` holds the field rules), and its trimmed strings come back trimmed.

use std::path::{Path, PathBuf};

use zc_contracts::T3ProjectFile;

pub use zc_vcs::project_file::T3_PROJECT_FILE_NAME;

/// `T3ProjectFileLoadError` (only logged).
#[derive(Debug)]
pub struct T3ProjectFileLoadError {
    /// `"read"` or `"decode"`.
    pub operation: &'static str,
    pub workspace_root: String,
    pub file_path: PathBuf,
    pub cause: String,
}

impl std::fmt::Display for T3ProjectFileLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Failed to {} {T3_PROJECT_FILE_NAME} at {}.", self.operation, self.file_path.display())
    }
}

/// `parseT3ProjectFile(contents)`: the decoded file, or `None` when it is malformed or invalid.
pub fn parse_t3_project_file(contents: &str) -> Option<T3ProjectFile> {
    zc_vcs::project_file::parse_t3_project_file(contents)?;
    let value = zc_core::lenient_json::parse_lenient_json(contents).ok()?;
    let mut file: T3ProjectFile = serde_json::from_value(value).ok()?;
    let trim = |text: &mut String| *text = text.trim().to_owned();
    if let Some(icon_path) = file.icon_path.as_mut() {
        trim(icon_path);
    }
    for script in file.scripts.iter_mut().flatten() {
        trim(&mut script.name);
        trim(&mut script.command);
        if let Some(preview_url) = script.preview_url.as_mut() {
            trim(preview_url);
        }
    }
    Some(file)
}

/// The `T3ProjectFileLoader` service (stateless).
#[derive(Debug, Clone, Copy, Default)]
pub struct T3ProjectFileLoader;

impl T3ProjectFileLoader {
    /// `load(workspaceRoot)`: never fails.
    pub async fn load(&self, workspace_root: impl AsRef<Path>) -> Option<T3ProjectFile> {
        let workspace_root = workspace_root.as_ref();
        let file_path = workspace_root.join(T3_PROJECT_FILE_NAME);
        let log = |operation: &'static str, cause: String| {
            let error = T3ProjectFileLoadError {
                operation,
                workspace_root: workspace_root.to_string_lossy().into_owned(),
                file_path: file_path.clone(),
                cause,
            };
            tracing::warn!(
                operation = error.operation,
                workspace_root = %error.workspace_root,
                file_path = %error.file_path.display(),
                error_tag = "T3ProjectFileLoadError",
                cause = %error.cause,
                "{error}"
            );
        };
        let raw = match tokio::fs::read(&file_path).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => {
                log("read", error.to_string());
                return None;
            }
        };
        let parsed = parse_t3_project_file(&raw);
        if parsed.is_none() {
            log("decode", "invalid t3.json".into());
        }
        parsed
    }
}
