//! `WorkspaceEntries` (`workspace/WorkspaceEntries.ts`):
//!
//! - `browse` (`filesystem.browse`): directories only, prefix-filtered, case-insensitive, dot
//!   directories only when asked for (`dir/` or `.prefix`); permission errors give an empty
//!   listing.
//! - `list` (`projects.listEntries`): with `directoryPath`, the immediate children of a
//!   directory inside the root (realpath containment, `.git` refused and hidden, symlinks and
//!   other special files skipped), each marked `ignored` by `git check-ignore`; without it, the
//!   whole cached path index.
//! - `search` / `searchContents`: the path index / content index of the normalized root.
//! - `refresh`: rescans whichever indexes of the root exist; a failed refresh drops the index so
//!   the next use rebuilds it.

use std::collections::HashSet;
use std::sync::Arc;

use zc_contracts::{
    FilesystemBrowseEntry, FilesystemBrowseResult, ProjectEntry, ProjectEntryKind, ProjectListEntriesResult, ProjectSearchContentsResult,
    ProjectSearchEntriesResult,
};
use zc_core::paths::expand_home_path;
use zc_core::{Defect, VcsProcess, VcsProcessInput};

use crate::backend::IndexVariant;
use crate::collate::locale_compare;
use crate::errors::{node_fs_defect, BrowseError, EntriesError, ReadDirectoryError, TaggedError};
use crate::index_map::SearchIndexMap;
use crate::paths::{self, WorkspacePaths};
use crate::platform::{is_explicit_relative_path, is_windows_absolute_path, NodePlatform};
use crate::search_index::ContentSearch;

/// `normalizeSearchQuery(input, { trimLeadingPattern: /^[@./]+/ })`.
pub fn normalize_search_query(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    trimmed.trim_start_matches(['@', '.', '/']).to_lowercase()
}

/// The `filesystem.browse` request (`FilesystemBrowseInput`, decoded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowseRequest {
    pub partial_path: String,
    pub cwd: Option<String>,
}

/// The `projects.searchEntries` request without `cwd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntrySearch {
    pub query: String,
    pub limit: usize,
    pub kind: Option<ProjectEntryKind>,
    pub image_only: bool,
}

/// The `WorkspaceEntries` service.
#[derive(Clone)]
pub struct WorkspaceEntries {
    paths: WorkspacePaths,
    indexes: SearchIndexMap,
    vcs: Arc<VcsProcess>,
    platform: NodePlatform,
}

impl std::fmt::Debug for WorkspaceEntries {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceEntries").field("indexes", &self.indexes).finish_non_exhaustive()
    }
}

/// `readdir(directory, { withFileTypes: true })` as `(name, is_directory, is_file)`, without
/// following symlinks (a `Dirent` of a symlink is neither).
fn read_dir_entries(directory: &str) -> std::io::Result<Vec<(String, bool, bool)>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        entries.push((entry.file_name().to_string_lossy().into_owned(), file_type.is_dir(), file_type.is_file()));
    }
    Ok(entries)
}

fn realpath(path: &str) -> Result<String, Defect> {
    std::fs::canonicalize(path)
        .map(|real| real.to_string_lossy().into_owned())
        .map_err(|error| node_fs_defect(&error, "realpath", path))
}

impl WorkspaceEntries {
    pub fn new(paths: WorkspacePaths, indexes: SearchIndexMap, vcs: Arc<VcsProcess>) -> Self {
        Self {
            paths,
            indexes,
            vcs,
            platform: NodePlatform::current(),
        }
    }

    /// The same service with another platform (for tests of the Windows-path rejection).
    pub fn with_platform(mut self, platform: NodePlatform) -> Self {
        self.platform = platform;
        self
    }

    pub fn indexes(&self) -> &SearchIndexMap {
        &self.indexes
    }

    pub fn paths(&self) -> &WorkspacePaths {
        &self.paths
    }

    fn resolve_browse_target(&self, input: &BrowseRequest) -> Result<String, BrowseError> {
        if self.platform != NodePlatform::Win32 && is_windows_absolute_path(&input.partial_path) {
            return Err(BrowseError::WindowsPathUnsupported {
                cwd: input.cwd.clone(),
                partial_path: input.partial_path.clone(),
                platform: self.platform.as_str().into(),
            });
        }
        if !is_explicit_relative_path(&input.partial_path) {
            return Ok(paths::resolve_one(&expand_home_path(&input.partial_path).to_string_lossy()));
        }
        let Some(cwd) = input.cwd.as_deref() else {
            return Err(BrowseError::CurrentProjectRequired {
                partial_path: input.partial_path.clone(),
            });
        };
        Ok(paths::resolve(&expand_home_path(cwd).to_string_lossy(), &input.partial_path))
    }

    /// `browse(input)`.
    pub async fn browse(&self, input: &BrowseRequest) -> Result<FilesystemBrowseResult, BrowseError> {
        let resolved = self.resolve_browse_target(input)?;
        let ends_with_separator = input.partial_path.ends_with('/') || input.partial_path.ends_with('\\') || input.partial_path == "~";
        let parent_path = if ends_with_separator { resolved.clone() } else { paths::dirname(&resolved) };
        let prefix = if ends_with_separator { String::new() } else { paths::basename(&resolved) };

        let listing = {
            let parent_path = parent_path.clone();
            tokio::task::spawn_blocking(move || read_dir_entries(&parent_path))
                .await
                .unwrap_or_else(|join| Err(std::io::Error::other(join.to_string())))
        };
        let dirents = match listing {
            Ok(dirents) => dirents,
            Err(error) if matches!(error.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM)) => Vec::new(),
            Err(error) => {
                return Err(BrowseError::ReadDirectory(ReadDirectoryError {
                    cwd: input.cwd.clone(),
                    partial_path: input.partial_path.clone(),
                    parent_path: parent_path.clone(),
                    cause: node_fs_defect(&error, "scandir", &parent_path),
                }))
            }
        };

        let show_hidden = ends_with_separator || prefix.starts_with('.');
        let lower_prefix = prefix.to_lowercase();
        let mut entries: Vec<FilesystemBrowseEntry> = dirents
            .into_iter()
            .filter(|(name, is_directory, _)| *is_directory && name.to_lowercase().starts_with(&lower_prefix) && (show_hidden || !name.starts_with('.')))
            .map(|(name, _, _)| FilesystemBrowseEntry {
                full_path: paths::join(&parent_path, &name),
                name,
            })
            .collect();
        entries.sort_by(|left, right| locale_compare(&left.name, &right.name));
        Ok(FilesystemBrowseResult { parent_path, entries })
    }

    /// `search(input)`.
    pub async fn search(&self, cwd: &str, input: &EntrySearch) -> Result<ProjectSearchEntriesResult, EntriesError> {
        let normalized_cwd = self.paths.normalize_workspace_root(cwd, false)?;
        let query = normalize_search_query(&input.query);
        let index = self.indexes.get(&normalized_cwd, IndexVariant::Paths).await?;
        Ok(index.search(&query, input.limit, input.kind, input.image_only).await?)
    }

    /// `searchContents(input)`.
    pub async fn search_contents(&self, cwd: &str, input: &ContentSearch) -> Result<ProjectSearchContentsResult, EntriesError> {
        let normalized_cwd = self.paths.normalize_workspace_root(cwd, false)?;
        let index = self.indexes.get(&normalized_cwd, IndexVariant::Content).await?;
        Ok(index.search_contents(input).await?)
    }

    /// `list(input)`.
    pub async fn list(&self, cwd: &str, directory_path: Option<&str>) -> Result<ProjectListEntriesResult, EntriesError> {
        let normalized_cwd = self.paths.normalize_workspace_root(cwd, false)?;
        let Some(directory_path) = directory_path else {
            let index = self.indexes.get(&normalized_cwd, IndexVariant::Paths).await?;
            return Ok(index.list().await?);
        };
        let to_error = |cause: Defect| {
            EntriesError::ReadDirectory(ReadDirectoryError {
                cwd: Some(normalized_cwd.clone()),
                partial_path: directory_path.to_owned(),
                parent_path: paths::resolve(&normalized_cwd, directory_path),
                cause,
            })
        };
        let (absolute_path, relative_path) = if directory_path.is_empty() {
            (normalized_cwd.clone(), String::new())
        } else {
            let target = self
                .paths
                .resolve_relative_path_within_root(&normalized_cwd, directory_path)
                .map_err(|error| to_error(error.to_defect()))?;
            (target.absolute_path, target.relative_path)
        };

        let children = {
            let normalized_cwd = normalized_cwd.clone();
            let relative_path = relative_path.clone();
            tokio::task::spawn_blocking(move || -> Result<Vec<ProjectEntry>, Defect> {
                let root = realpath(&normalized_cwd)?;
                let directory = realpath(&absolute_path)?;
                let relative = paths::relative(&root, &directory);
                if relative == ".."
                    || relative.starts_with("../")
                    || paths::is_absolute(&relative)
                    || relative.split('/').any(|part| part == ".git")
                    || relative_path.split('/').any(|part| part == ".git")
                {
                    return Err(Defect::error("Error", "Directory must be inside the workspace and outside .git."));
                }
                let children = read_dir_entries(&directory).map_err(|error| node_fs_defect(&error, "scandir", &directory))?;
                Ok(children
                    .into_iter()
                    .filter(|(name, is_directory, is_file)| name != ".git" && (*is_directory || *is_file))
                    .map(|(name, is_directory, _)| ProjectEntry {
                        path: if relative_path.is_empty() { name } else { format!("{relative_path}/{name}") },
                        kind: if is_directory { ProjectEntryKind::Directory } else { ProjectEntryKind::File },
                        ignored: None,
                    })
                    .collect())
            })
            .await
            .unwrap_or_else(|join| Err(Defect::error("Error", join.to_string())))
            .map_err(to_error)?
        };

        // Through stdin so large directories cannot exceed the argument limit. Ignore
        // classification is optional (non-git workspaces, git missing).
        let mut ignored: HashSet<String> = HashSet::new();
        for chunk in children.chunks(1000) {
            let mut stdin: String = chunk.iter().map(|entry| entry.path.as_str()).collect::<Vec<_>>().join("\0");
            stdin.push('\0');
            let mut input = VcsProcessInput::new(
                "WorkspaceEntries.list",
                "git",
                ["-c", "core.fsmonitor=false", "check-ignore", "-z", "--stdin"],
                &normalized_cwd,
            );
            input.stdin = Some(stdin);
            input.allow_non_zero_exit = true;
            input.timeout_ms = Some(10_000);
            input.max_output_bytes = Some(16 * 1024 * 1024);
            let Ok(output) = self.vcs.run(input).await else { break };
            if output.exit_code != 0 && output.exit_code != 1 {
                break;
            }
            ignored.extend(output.stdout.split('\0').map(str::to_owned));
        }
        Ok(ProjectListEntriesResult {
            entries: children
                .into_iter()
                .map(|entry| {
                    if ignored.contains(&entry.path) {
                        ProjectEntry { ignored: Some(true), ..entry }
                    } else {
                        entry
                    }
                })
                .collect(),
            truncated: false,
        })
    }

    /// `refresh(cwd)`: never fails; a broken index is dropped so the next use rebuilds it.
    pub async fn refresh(&self, cwd: &str) {
        let normalized_cwd = self.paths.normalize_workspace_root(cwd, false).unwrap_or_else(|_| cwd.to_owned());
        for variant in IndexVariant::ALL {
            if !self.indexes.has(&normalized_cwd, variant) {
                continue;
            }
            let refreshed = match self.indexes.get(&normalized_cwd, variant).await {
                Ok(index) => index.refresh().await,
                Err(error) => Err(error),
            };
            if let Err(error) = refreshed {
                tracing::warn!(cwd, variant = variant.as_str(), error = %error, "Failed to refresh workspace search index");
                self.indexes.invalidate(&normalized_cwd, variant);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_search_queries() {
        assert_eq!(normalize_search_query("  @./Src/Comp  "), "src/comp");
        assert_eq!(normalize_search_query("   "), "");
        assert_eq!(normalize_search_query("Composer.tsx"), "composer.tsx");
    }
}
