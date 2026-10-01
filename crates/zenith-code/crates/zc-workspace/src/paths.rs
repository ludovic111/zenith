//! `WorkspacePaths` (`workspace/WorkspacePaths.ts`): normalizing a workspace root and resolving
//! workspace-relative paths without leaving it (lexically; the realpath checks live in
//! [`crate::file_system`] and [`crate::entries`]).
//!
//! Also the Node `path` helpers the module relies on (`path.resolve`, `path.relative`,
//! `path.isAbsolute`, POSIX flavour).

use std::path::{Path, PathBuf};

use zc_core::paths::{expand_home_path, normalize_lexically};

use crate::errors::{platform_error_defect, StatPhase, WorkspacePathOutsideRootError, WorkspaceRootError};

/// `path.isAbsolute` (POSIX).
pub fn is_absolute(path: &str) -> bool {
    path.starts_with('/')
}

/// `path.resolve(base, path)`: `path` if absolute, else joined onto `base` (itself resolved
/// against the process cwd), with `.` and `..` folded lexically. Trailing slashes are dropped.
pub fn resolve(base: &str, path: &str) -> String {
    let joined: PathBuf = if is_absolute(path) {
        PathBuf::from(path)
    } else if is_absolute(base) {
        Path::new(base).join(path)
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")).join(base).join(path)
    };
    normalize_lexically(&joined).to_string_lossy().into_owned()
}

/// `path.resolve(path)`.
pub fn resolve_one(path: &str) -> String {
    resolve("", path)
}

/// `path.relative(from, to)` (POSIX): both are resolved first; `""` when they are the same.
pub fn relative(from: &str, to: &str) -> String {
    let from = resolve_one(from);
    let to = resolve_one(to);
    let from_parts: Vec<&str> = from.split('/').filter(|part| !part.is_empty()).collect();
    let to_parts: Vec<&str> = to.split('/').filter(|part| !part.is_empty()).collect();
    let common = from_parts.iter().zip(&to_parts).take_while(|(left, right)| left == right).count();
    let mut parts: Vec<&str> = std::iter::repeat_n("..", from_parts.len() - common).collect();
    parts.extend_from_slice(&to_parts[common..]);
    parts.join("/")
}

/// `path.dirname` (POSIX).
pub fn dirname(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".into();
    }
    match trimmed.rfind('/') {
        None => ".".into(),
        Some(index) => {
            let head = trimmed[..index].trim_end_matches('/');
            if head.is_empty() {
                "/".into()
            } else {
                head.into()
            }
        }
    }
}

/// `path.basename` (POSIX).
pub fn basename(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(index) => trimmed[index + 1..].to_owned(),
        None => trimmed.to_owned(),
    }
}

/// `path.join(a, b)` (POSIX, normalized).
pub fn join(base: &str, name: &str) -> String {
    let joined = format!("{base}/{name}");
    let normalized = normalize_lexically(Path::new(&joined)).to_string_lossy().into_owned();
    if !is_absolute(base) && !is_absolute(&normalized) && normalized.starts_with("./") {
        normalized[2..].to_owned()
    } else {
        normalized
    }
}

/// The file-system calls `normalizeWorkspaceRoot` makes, injectable for tests (the TS tests
/// provide a failing `FileSystem.stat`).
#[derive(Clone, Copy)]
pub struct RootFs {
    /// `Ok(None)` when the path does not exist; `Ok(Some(is_directory))` otherwise.
    pub stat: fn(&str) -> std::io::Result<Option<bool>>,
    pub make_directory_all: fn(&str) -> std::io::Result<()>,
}

fn std_stat(path: &str) -> std::io::Result<Option<bool>> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata.is_dir())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn std_make_directory_all(path: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

impl Default for RootFs {
    fn default() -> Self {
        Self {
            stat: std_stat,
            make_directory_all: std_make_directory_all,
        }
    }
}

/// The `WorkspacePaths` service.
#[derive(Clone, Copy, Default)]
pub struct WorkspacePaths {
    fs: RootFs,
}

impl std::fmt::Debug for WorkspacePaths {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspacePaths").finish_non_exhaustive()
    }
}

impl WorkspacePaths {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_fs(fs: RootFs) -> Self {
        Self { fs }
    }

    /// `normalizeWorkspaceRoot`: trims, expands `~`, resolves, and checks that the result is an
    /// existing directory (creating it first with `create_if_missing`).
    pub fn normalize_workspace_root(&self, workspace_root: &str, create_if_missing: bool) -> Result<String, WorkspaceRootError> {
        let normalized = resolve_one(&expand_home_path(workspace_root.trim()).to_string_lossy());
        let stat = |phase: StatPhase| {
            (self.fs.stat)(&normalized).map_err(|error| WorkspaceRootError::StatFailed {
                workspace_root: workspace_root.to_owned(),
                normalized_workspace_root: normalized.clone(),
                phase,
                cause: platform_error_defect(&error, "stat", "stat", &normalized),
            })
        };
        let mut is_directory = stat(StatPhase::ValidateExisting)?;
        if is_directory.is_none() && create_if_missing {
            (self.fs.make_directory_all)(&normalized).map_err(|error| WorkspaceRootError::CreateFailed {
                workspace_root: workspace_root.to_owned(),
                normalized_workspace_root: normalized.clone(),
                cause: platform_error_defect(&error, "makeDirectory", "mkdir", &normalized),
            })?;
            is_directory = stat(StatPhase::VerifyCreated)?;
        }
        match is_directory {
            None => Err(WorkspaceRootError::NotExists {
                workspace_root: workspace_root.to_owned(),
                normalized_workspace_root: normalized,
            }),
            Some(false) => Err(WorkspaceRootError::NotDirectory {
                workspace_root: workspace_root.to_owned(),
                normalized_workspace_root: normalized,
            }),
            Some(true) => Ok(normalized),
        }
    }

    /// `resolveRelativePathWithinRoot`: rejects absolute paths and anything that resolves to the
    /// root itself or outside it. Returns `(absolutePath, relativePath)`, the latter POSIX.
    pub fn resolve_relative_path_within_root(&self, workspace_root: &str, relative_path: &str) -> Result<ResolvedWorkspacePath, WorkspacePathOutsideRootError> {
        resolve_relative_path_within_root(workspace_root, relative_path)
    }
}

/// A path inside a workspace root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedWorkspacePath {
    pub absolute_path: String,
    pub relative_path: String,
}

/// [`WorkspacePaths::resolve_relative_path_within_root`] as a free function.
pub fn resolve_relative_path_within_root(workspace_root: &str, relative_path: &str) -> Result<ResolvedWorkspacePath, WorkspacePathOutsideRootError> {
    let outside = || WorkspacePathOutsideRootError {
        workspace_root: workspace_root.to_owned(),
        relative_path: relative_path.to_owned(),
    };
    let normalized_input = relative_path.trim();
    if is_absolute(normalized_input) {
        return Err(outside());
    }
    let absolute_path = resolve(workspace_root, normalized_input);
    let relative_to_root = relative(workspace_root, &absolute_path).replace('\\', "/");
    if relative_to_root.is_empty()
        || relative_to_root == "."
        || relative_to_root.starts_with("../")
        || relative_to_root == ".."
        || is_absolute(&relative_to_root)
    {
        return Err(outside());
    }
    Ok(ResolvedWorkspacePath {
        absolute_path,
        relative_path: relative_to_root,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::TaggedError;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::Builder::new().prefix("zc-workspace-paths-").tempdir().unwrap()
    }

    fn path_of(dir: &tempfile::TempDir) -> String {
        dir.path().to_string_lossy().into_owned()
    }

    #[test]
    fn node_path_helpers() {
        assert_eq!(resolve("/a/b", "../c/./d/"), "/a/c/d");
        assert_eq!(resolve("/a", "/x/y"), "/x/y");
        assert_eq!(relative("/a/b", "/a/b"), "");
        assert_eq!(relative("/a/b", "/a/b/c/d"), "c/d");
        assert_eq!(relative("/a/b", "/a/x"), "../x");
        assert_eq!(relative("/a/b", "/"), "../..");
        assert_eq!(dirname("/a/b/c"), "/a/b");
        assert_eq!(dirname("/a"), "/");
        assert_eq!(dirname("/"), "/");
        assert_eq!(dirname("a"), ".");
        assert_eq!(dirname("/a/b/"), "/a");
        assert_eq!(basename("/a/b/"), "b");
        assert_eq!(join("/a/b", "c"), "/a/b/c");
        assert_eq!(join("/", "c"), "/c");
    }

    // WorkspacePaths.test.ts: "resolves an existing directory"
    #[test]
    fn resolves_an_existing_directory() {
        let dir = temp_dir();
        let cwd = path_of(&dir);
        assert_eq!(WorkspacePaths::new().normalize_workspace_root(&cwd, false).unwrap(), cwd);
        // Whitespace around the root is trimmed.
        assert_eq!(WorkspacePaths::new().normalize_workspace_root(&format!("  {cwd}  "), false).unwrap(), cwd);
    }

    // "rejects missing directories"
    #[test]
    fn rejects_missing_directories() {
        let dir = temp_dir();
        let error = WorkspacePaths::new()
            .normalize_workspace_root(&format!("{}/missing", path_of(&dir)), false)
            .unwrap_err();
        assert!(error.message().contains("Workspace root does not exist:"));
    }

    // "creates missing directories when createIfMissing is enabled"
    #[test]
    fn creates_missing_directories() {
        let dir = temp_dir();
        let missing = format!("{}/nested/new-project", path_of(&dir));
        let resolved = WorkspacePaths::new().normalize_workspace_root(&missing, true).unwrap();
        assert_eq!(resolved, missing);
        assert!(std::fs::metadata(&resolved).unwrap().is_dir());
    }

    // "rejects file paths"
    #[test]
    fn rejects_file_paths() {
        let dir = temp_dir();
        let file = format!("{}/README.md", path_of(&dir));
        std::fs::write(&file, "# hi\n").unwrap();
        let error = WorkspacePaths::new().normalize_workspace_root(&file, false).unwrap_err();
        assert!(error.message().contains("Workspace root is not a directory:"));
    }

    // "preserves non-NotFound stat failures while validating the root"
    #[test]
    fn preserves_stat_failures_while_validating() {
        let paths = WorkspacePaths::with_fs(RootFs {
            stat: |_| Err(std::io::Error::from_raw_os_error(libc::EACCES)),
            make_directory_all: |_| Ok(()),
        });
        let root = " ./permission-denied ";
        let error = paths.normalize_workspace_root(root, false).unwrap_err();
        match error {
            WorkspaceRootError::StatFailed {
                workspace_root,
                normalized_workspace_root,
                phase,
                ..
            } => {
                assert_eq!(workspace_root, root);
                assert_eq!(normalized_workspace_root, resolve_one("./permission-denied"));
                assert_eq!(phase, StatPhase::ValidateExisting);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    // "preserves stat failures while verifying a newly created root"
    #[test]
    fn preserves_stat_failures_while_verifying_created_root() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let paths = WorkspacePaths::with_fs(RootFs {
            stat: |_| {
                if CALLS.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(None)
                } else {
                    Err(std::io::Error::from_raw_os_error(libc::EACCES))
                }
            },
            make_directory_all: |_| Ok(()),
        });
        let root = " ./created-then-unreadable ";
        let error = paths.normalize_workspace_root(root, true).unwrap_err();
        match error {
            WorkspaceRootError::StatFailed {
                workspace_root,
                normalized_workspace_root,
                phase,
                ..
            } => {
                assert_eq!(workspace_root, root);
                assert_eq!(normalized_workspace_root, resolve_one("./created-then-unreadable"));
                assert_eq!(phase, StatPhase::VerifyCreated);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    // "resolves relative paths inside the workspace root"
    #[test]
    fn resolves_relative_paths_inside_the_root() {
        let dir = temp_dir();
        let cwd = path_of(&dir);
        assert_eq!(
            resolve_relative_path_within_root(&cwd, "plans/effect-rpc.md").unwrap(),
            ResolvedWorkspacePath {
                absolute_path: format!("{cwd}/plans/effect-rpc.md"),
                relative_path: "plans/effect-rpc.md".into(),
            }
        );
        assert_eq!(resolve_relative_path_within_root(&cwd, " ./a/../b.md ").unwrap().relative_path, "b.md");
    }

    // "rejects paths that escape the workspace root"
    #[test]
    fn rejects_paths_that_escape_the_root() {
        let dir = temp_dir();
        let cwd = path_of(&dir);
        for escaping in ["../escape.md", "/etc/passwd", ".", "", "a/../..", "a/../../b"] {
            let error = resolve_relative_path_within_root(&cwd, escaping).unwrap_err();
            assert_eq!(error.message(), format!("Workspace file path must be relative to the project root: {escaping}"));
        }
    }
}
