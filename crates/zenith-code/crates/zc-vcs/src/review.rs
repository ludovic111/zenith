//! `review/ReviewService.ts`: diff previews and file expansion for the review panel, limited
//! to cwds inside the server's workspace root or its worktrees directory.

use std::path::{Component, Path, PathBuf};

use crate::contracts::*;
use crate::driver_core::GitVcsDriver;
use crate::errors::{ReviewDiffPreviewError, VcsError, VcsRepositoryDetectionError, VcsUnsupportedOperationError};
use crate::registry::VcsDriverRegistry;

/// `ReviewService`.
#[derive(Clone)]
pub struct ReviewService {
    workspace_root: PathBuf,
    worktrees_dir: PathBuf,
    registry: VcsDriverRegistry,
    git: GitVcsDriver,
}

/// `isWithinRoot`: `path.relative(root, candidate)` is empty, or neither starts with `..` nor
/// is absolute (so a child literally named `..x` counts as outside, like TS).
fn is_within_root(candidate: &Path, root: &Path) -> bool {
    let Ok(rest) = candidate.strip_prefix(root) else {
        return false;
    };
    match rest.components().next() {
        None => true,
        Some(Component::Normal(first)) => !first.to_string_lossy().starts_with(".."),
        Some(_) => false,
    }
}

impl ReviewService {
    pub fn new(workspace_root: impl Into<PathBuf>, worktrees_dir: impl Into<PathBuf>, registry: VcsDriverRegistry, git: GitVcsDriver) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            worktrees_dir: worktrees_dir.into(),
            registry,
            git,
        }
    }

    /// `canonicalizePath`: the real path, the resolved path when it does not exist, and an
    /// error for anything else.
    async fn canonicalize(value: &Path) -> Result<PathBuf, VcsError> {
        let resolved = zc_core::paths::resolve_path(value);
        match tokio::fs::canonicalize(&resolved).await {
            Ok(real) => Ok(real),
            Err(io) if io.kind() == std::io::ErrorKind::NotFound => Ok(resolved),
            Err(io) => Err(VcsError::RepositoryDetection(VcsRepositoryDetectionError {
                operation: "ReviewService.assertWorkspaceBoundCwd.canonicalizePath".into(),
                cwd: resolved.to_string_lossy().into_owned(),
                detail: "Failed to resolve a path while validating the review workspace.".into(),
                cause: Some(crate::errors::platform_error_defect("realPath", &resolved.to_string_lossy(), &io)),
            })),
        }
    }

    async fn assert_workspace_bound_cwd(&self, operation: &str, cwd: &str) -> Result<(), VcsError> {
        let candidate = Self::canonicalize(Path::new(cwd)).await?;
        let workspace_root = Self::canonicalize(&self.workspace_root).await?;
        let worktrees_root = Self::canonicalize(&self.worktrees_dir).await?;
        if is_within_root(&candidate, &workspace_root) || is_within_root(&candidate, &worktrees_root) {
            return Ok(());
        }
        Err(VcsError::RepositoryDetection(VcsRepositoryDetectionError {
            operation: operation.into(),
            cwd: cwd.into(),
            detail: if operation == "ReviewService.getDiffPreview" {
                "Review diff preview cwd must stay within the configured workspace root.".into()
            } else {
                "Review diff file contents cwd must stay within the configured workspace root.".into()
            },
            cause: None,
        }))
    }

    /// `getDiffPreview(input)`.
    pub async fn get_diff_preview(&self, input: &ReviewDiffPreviewInput) -> Result<ReviewDiffPreviewResult, ReviewDiffPreviewError> {
        self.assert_workspace_bound_cwd("ReviewService.getDiffPreview", &input.cwd).await?;
        let Some(handle) = self.registry.detect(&input.cwd, Some(RequestedVcsKind::Auto)).await? else {
            return Ok(ReviewDiffPreviewResult::empty(&input.cwd));
        };
        if handle.driver.supports_diff_preview() {
            return Ok(handle.driver.get_diff_preview(input).await?);
        }
        if handle.kind == VcsDriverKind::Git {
            return Ok(self.git.get_review_diff_preview(input).await?);
        }
        Err(VcsError::UnsupportedOperation(VcsUnsupportedOperationError::new(
            "ReviewService.getDiffPreview",
            handle.kind,
            format!("The {} VCS driver does not support review diff previews.", handle.kind),
        ))
        .into())
    }

    /// `getDiffFileContents(input)`.
    pub async fn get_diff_file_contents(&self, input: &ReviewDiffFileContentsInput) -> Result<ReviewDiffFileContentsResult, ReviewDiffPreviewError> {
        self.assert_workspace_bound_cwd("ReviewService.getDiffFileContents", &input.cwd).await?;
        let handle = self.registry.detect(&input.cwd, Some(RequestedVcsKind::Auto)).await?;
        let kind = handle.map(|h| h.kind).unwrap_or(VcsDriverKind::Unknown);
        if kind != VcsDriverKind::Git {
            return Err(VcsError::UnsupportedOperation(VcsUnsupportedOperationError::new(
                "ReviewService.getDiffFileContents",
                kind,
                "Unchanged diff expansion currently requires a Git repository.",
            ))
            .into());
        }
        Ok(self.git.get_review_diff_file_contents(input).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_containment_follows_path_relative() {
        let root = Path::new("/w");
        assert!(is_within_root(Path::new("/w"), root));
        assert!(is_within_root(Path::new("/w/a/b"), root));
        assert!(!is_within_root(Path::new("/w/..x"), root));
        assert!(!is_within_root(Path::new("/other"), root));
        assert!(!is_within_root(Path::new("/wx"), root));
    }
}
