//! Where an instance's Codex state lives (`provider/Drivers/CodexHomeLayout.ts`).
//!
//! `direct`: one `CODEX_HOME`. `authOverlay` (a shadow home is configured): the runtime runs in
//! the shadow home, which keeps its own `auth.json` (and `models_cache.json`) and links every
//! other entry of the shared home, so two accounts share sessions, skills, config, … Sessions
//! are continuable across instances that share the same shared home (`codex:home:<path>`).

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use zc_contracts::CodexSettings;
use zc_ports::provider::ProviderContinuationIdentity;

/// `CodexHomeLayout`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexHomeLayout {
    pub mode: CodexHomeMode,
    pub shared_home_path: PathBuf,
    /// The `CODEX_HOME` to run with; `None` = Codex's default (no home configured).
    pub effective_home_path: Option<PathBuf>,
    pub continuation_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexHomeMode {
    Direct,
    AuthOverlay,
}

const KNOWN_SHARED_DIRECTORIES: &[&str] = &[
    "sessions",
    "archived_sessions",
    "sqlite",
    "shell_snapshots",
    "worktrees",
    "skills",
    "plugins",
    "cache",
    "logs",
    "mcp-oauth-locks",
];

const PRIVATE_ENTRY_NAMES: &[&str] = &["auth.json", "models_cache.json"];
const SHADOW_LOCAL_ENTRY_NAMES: &[&str] = &["log", "memories", "tmp"];
const REPLACEABLE_SHARED_RUNTIME_DIRECTORIES: &[&str] = &["mcp-oauth-locks"];

/// Node's `path.resolve` (lexical, against the current directory).
fn resolve(path: &Path) -> PathBuf {
    zc_core::paths::resolve_path(path)
}

fn resolve_home_path(value: &str) -> PathBuf {
    let expanded = if value.trim().is_empty() {
        zc_core::paths::home_dir().join(".codex")
    } else {
        zc_core::expand_home_path(value)
    };
    resolve(&expanded)
}

/// `resolveCodexHomeLayout`.
pub fn resolve_codex_home_layout(config: &CodexSettings) -> CodexHomeLayout {
    let shared_home_path = resolve_home_path(&config.home_path);
    let continuation_key = format!("codex:home:{}", shared_home_path.display());
    let shadow = config.shadow_home_path.trim();
    if shadow.is_empty() {
        return CodexHomeLayout {
            mode: CodexHomeMode::Direct,
            effective_home_path: (!config.home_path.trim().is_empty()).then(|| shared_home_path.clone()),
            shared_home_path,
            continuation_key,
        };
    }
    CodexHomeLayout {
        mode: CodexHomeMode::AuthOverlay,
        effective_home_path: Some(resolve(&zc_core::expand_home_path(shadow))),
        shared_home_path,
        continuation_key,
    }
}

/// `codexContinuationIdentity`.
pub fn codex_continuation_identity(layout: &CodexHomeLayout) -> ProviderContinuationIdentity {
    ProviderContinuationIdentity {
        driver_kind: zc_ports::contracts::ProviderDriverKind::new(crate::DRIVER_KIND),
        continuation_key: layout.continuation_key.clone(),
    }
}

/// `CodexShadowHomeError`.
#[derive(Debug, thiserror::Error)]
pub enum CodexShadowHomeError {
    #[error("Codex shadow home filesystem operation '{operation}' failed for '{}'{}.", path.display(), target_path.as_ref().map(|target| format!(" to '{}'", target.display())).unwrap_or_default())]
    FileSystem {
        shared_home_path: PathBuf,
        effective_home_path: PathBuf,
        operation: &'static str,
        path: PathBuf,
        target_path: Option<PathBuf>,
        entry_name: Option<String>,
        cause: io::Error,
    },
    #[error("Codex shadow home path '{}' must be different from the shared home path '{}'.", effective_home_path.display(), shared_home_path.display())]
    PathConflict { shared_home_path: PathBuf, effective_home_path: PathBuf },
    #[error("Cannot create Codex shadow home entry '{entry_name}' because '{}' already exists and is not a symlink.", link_path.display())]
    EntryConflict {
        shared_home_path: PathBuf,
        effective_home_path: PathBuf,
        entry_name: String,
        link_path: PathBuf,
        target_path: PathBuf,
    },
    #[error("Codex shadow home private entry '{entry_name}' at '{}' must be a real file, not a symlink.", path.display())]
    PrivateEntrySymlink {
        shared_home_path: PathBuf,
        effective_home_path: PathBuf,
        entry_name: String,
        path: PathBuf,
    },
}

impl CodexShadowHomeError {
    pub fn tag(&self) -> &'static str {
        match self {
            CodexShadowHomeError::FileSystem { .. } => "CodexShadowHomeFileSystemError",
            CodexShadowHomeError::PathConflict { .. } => "CodexShadowHomePathConflictError",
            CodexShadowHomeError::EntryConflict { .. } => "CodexShadowHomeEntryConflictError",
            CodexShadowHomeError::PrivateEntrySymlink { .. } => "CodexShadowHomePrivateEntrySymlinkError",
        }
    }
}

enum LinkState {
    Missing,
    NotSymlink,
    Symlink(PathBuf),
}

struct Ctx<'a> {
    shared: &'a Path,
    effective: &'a Path,
}

impl Ctx<'_> {
    fn fs_error(&self, operation: &'static str, path: &Path, target: Option<&Path>, entry: Option<&str>, cause: io::Error) -> CodexShadowHomeError {
        CodexShadowHomeError::FileSystem {
            shared_home_path: self.shared.to_path_buf(),
            effective_home_path: self.effective.to_path_buf(),
            operation,
            path: path.to_path_buf(),
            target_path: target.map(Path::to_path_buf),
            entry_name: entry.map(str::to_owned),
            cause,
        }
    }

    fn read_link_state(&self, entry: &str, link: &Path) -> Result<LinkState, CodexShadowHomeError> {
        match std::fs::read_link(link) {
            Ok(target) => Ok(LinkState::Symlink(target)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(LinkState::Missing),
            // EINVAL: the path exists and is not a symlink.
            Err(error) if error.raw_os_error() == Some(libc::EINVAL) => Ok(LinkState::NotSymlink),
            Err(error) => Err(self.fs_error("readLink", link, None, Some(entry), error)),
        }
    }

    fn remove(&self, path: &Path, entry: &str, recursive: bool) -> Result<(), CodexShadowHomeError> {
        let result = if recursive && path.symlink_metadata().is_ok_and(|metadata| metadata.is_dir()) {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
        result.map_err(|error| self.fs_error("remove", path, None, Some(entry), error))
    }

    fn symlink(&self, target: &Path, link: &Path, entry: &str) -> Result<(), CodexShadowHomeError> {
        #[cfg(unix)]
        let result = std::os::unix::fs::symlink(target, link);
        #[cfg(not(unix))]
        let result: io::Result<()> = Err(io::Error::new(io::ErrorKind::Unsupported, "symlinks need a Unix host"));
        result.map_err(|error| self.fs_error("symlink", link, Some(target), Some(entry), error))
    }

    fn ensure_symlink(&self, entry: &str) -> Result<(), CodexShadowHomeError> {
        let target = self.shared.join(entry);
        let link = self.effective.join(entry);
        match self.read_link_state(entry, &link)? {
            LinkState::NotSymlink => {
                if !REPLACEABLE_SHARED_RUNTIME_DIRECTORIES.contains(&entry) {
                    return Err(CodexShadowHomeError::EntryConflict {
                        shared_home_path: self.shared.to_path_buf(),
                        effective_home_path: self.effective.to_path_buf(),
                        entry_name: entry.to_owned(),
                        link_path: link,
                        target_path: target,
                    });
                }
                self.remove(&link, entry, true)?;
                self.symlink(&target, &link, entry)
            }
            LinkState::Missing => self.symlink(&target, &link, entry),
            LinkState::Symlink(existing) => {
                let base = link.parent().unwrap_or(Path::new("/"));
                if resolve(&base.join(existing)) != target {
                    self.remove(&link, entry, false)?;
                    self.symlink(&target, &link, entry)?;
                }
                Ok(())
            }
        }
    }
}

/// `materializeCodexShadowHome`: creates the shadow home, links the shared entries into it,
/// removes links to private entries, and refuses a linked `auth.json`.
pub fn materialize_codex_shadow_home(layout: &CodexHomeLayout) -> Result<(), CodexShadowHomeError> {
    if layout.mode != CodexHomeMode::AuthOverlay {
        return Ok(());
    }
    let Some(effective) = layout.effective_home_path.as_deref() else {
        return Ok(());
    };
    let shared = layout.shared_home_path.as_path();
    if shared == effective {
        return Err(CodexShadowHomeError::PathConflict {
            shared_home_path: shared.to_path_buf(),
            effective_home_path: effective.to_path_buf(),
        });
    }
    let ctx = Ctx { shared, effective };
    let make_directory = |path: &Path| std::fs::create_dir_all(path).map_err(|error| ctx.fs_error("makeDirectory", path, None, None, error));
    make_directory(shared)?;
    make_directory(effective)?;
    for directory in KNOWN_SHARED_DIRECTORIES {
        make_directory(&shared.join(directory))?;
    }
    let read = std::fs::read_dir(shared).map_err(|error| ctx.fs_error("readDirectory", shared, None, None, error))?;
    let mut entries: BTreeSet<String> = KNOWN_SHARED_DIRECTORIES.iter().map(|name| (*name).to_owned()).collect();
    let mut ordered: Vec<String> = KNOWN_SHARED_DIRECTORIES.iter().map(|name| (*name).to_owned()).collect();
    for entry in read {
        let entry = entry.map_err(|error| ctx.fs_error("readDirectory", shared, None, None, error))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !PRIVATE_ENTRY_NAMES.contains(&name.as_str()) && !SHADOW_LOCAL_ENTRY_NAMES.contains(&name.as_str()) && entries.insert(name.clone()) {
            ordered.push(name);
        }
    }
    for private in PRIVATE_ENTRY_NAMES.iter().filter(|name| **name != "auth.json") {
        let path = effective.join(private);
        if let LinkState::Symlink(_) = ctx.read_link_state(private, &path)? {
            ctx.remove(&path, private, false)?;
        }
    }
    for entry in &ordered {
        ctx.ensure_symlink(entry)?;
    }
    let auth = effective.join("auth.json");
    if let LinkState::Symlink(_) = ctx.read_link_state("auth.json", &auth)? {
        return Err(CodexShadowHomeError::PrivateEntrySymlink {
            shared_home_path: shared.to_path_buf(),
            effective_home_path: effective.to_path_buf(),
            entry_name: "auth.json".to_owned(),
            path: auth,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(home: &Path, shadow: Option<&Path>) -> CodexSettings {
        crate::model::from_json(serde_json::json!({
            "homePath": home.to_string_lossy(),
            "shadowHomePath": shadow.map(|shadow| shadow.to_string_lossy().into_owned()).unwrap_or_default(),
        }))
    }

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn direct_home_without_a_shadow() {
        let home = tempfile::tempdir().unwrap();
        let layout = resolve_codex_home_layout(&settings(home.path(), None));
        assert_eq!(layout.mode, CodexHomeMode::Direct);
        assert_eq!(layout.shared_home_path, home.path());
        assert_eq!(layout.effective_home_path.as_deref(), Some(home.path()));
        assert_eq!(layout.continuation_key, format!("codex:home:{}", home.path().display()));
        let default = resolve_codex_home_layout(&crate::model::from_json(serde_json::json!({})));
        assert_eq!(default.effective_home_path, None);
        assert_eq!(default.shared_home_path, zc_core::paths::home_dir().join(".codex"));
    }

    #[test]
    fn shared_home_continues_and_shadow_home_runs() {
        let shared = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let shadow = root.path().join("shadow");
        let layout = resolve_codex_home_layout(&settings(shared.path(), Some(&shadow)));
        assert_eq!(layout.mode, CodexHomeMode::AuthOverlay);
        assert_eq!(layout.shared_home_path, shared.path());
        assert_eq!(layout.effective_home_path.as_deref(), Some(shadow.as_path()));
        assert_eq!(layout.continuation_key, format!("codex:home:{}", shared.path().display()));
    }

    #[cfg(unix)]
    #[test]
    fn materializes_shared_links_and_private_auth() {
        let shared = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let shadow = root.path().join("shadow");
        std::fs::create_dir_all(shared.path().join("sessions")).unwrap();
        write(&shared.path().join("config.toml"), "model = \"gpt-5-codex\"\n");
        write(&shared.path().join("models_cache.json"), "{\"models\":[\"shared\"]}\n");
        write(&shared.path().join("auth.json"), "{\"shared\":true}\n");
        write(&shadow.join("auth.json"), "{\"shadow\":true}\n");
        std::os::unix::fs::symlink(shared.path().join("models_cache.json"), shadow.join("models_cache.json")).unwrap();

        materialize_codex_shadow_home(&resolve_codex_home_layout(&settings(shared.path(), Some(&shadow)))).unwrap();

        assert_eq!(std::fs::read_link(shadow.join("sessions")).unwrap(), shared.path().join("sessions"));
        assert_eq!(std::fs::read_link(shadow.join("config.toml")).unwrap(), shared.path().join("config.toml"));
        assert_eq!(
            std::fs::read_link(shadow.join("mcp-oauth-locks")).unwrap(),
            shared.path().join("mcp-oauth-locks")
        );
        assert!(!shadow.join("models_cache.json").exists());
        assert!(std::fs::read_link(shadow.join("auth.json")).is_err());
        assert!(std::fs::read_to_string(shadow.join("auth.json")).unwrap().contains("shadow"));
    }

    #[cfg(unix)]
    #[test]
    fn replaces_local_mcp_oauth_locks_with_the_shared_directory() {
        let shared = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let shadow = root.path().join("shadow");
        write(&shared.path().join("mcp-oauth-locks/file-store.lock"), "");
        write(&shadow.join("mcp-oauth-locks/file-store.lock"), "");
        materialize_codex_shadow_home(&resolve_codex_home_layout(&settings(shared.path(), Some(&shadow)))).unwrap();
        assert_eq!(
            std::fs::read_link(shadow.join("mcp-oauth-locks")).unwrap(),
            shared.path().join("mcp-oauth-locks")
        );
        assert!(shared.path().join("mcp-oauth-locks/file-store.lock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn accepts_shadow_local_runtime_directories() {
        let shared = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let shadow = root.path().join("shadow");
        for name in ["log", "memories", "tmp"] {
            std::fs::create_dir_all(shared.path().join(name)).unwrap();
            std::fs::create_dir_all(shadow.join(name)).unwrap();
        }
        write(&shared.path().join("config.toml"), "model = \"gpt-5-codex\"\n");
        write(&shadow.join("auth.json"), "{\"shadow\":true}\n");
        materialize_codex_shadow_home(&resolve_codex_home_layout(&settings(shared.path(), Some(&shadow)))).unwrap();
        assert_eq!(std::fs::read_link(shadow.join("config.toml")).unwrap(), shared.path().join("config.toml"));
        for name in ["log", "memories", "tmp"] {
            assert!(std::fs::read_link(shadow.join(name)).is_err());
        }
    }

    #[test]
    fn rejects_a_shadow_home_equal_to_the_shared_home() {
        let shared = tempfile::tempdir().unwrap();
        let error = materialize_codex_shadow_home(&resolve_codex_home_layout(&settings(shared.path(), Some(shared.path())))).unwrap_err();
        assert_eq!(error.tag(), "CodexShadowHomePathConflictError");
        assert_eq!(
            error.to_string(),
            format!(
                "Codex shadow home path '{0}' must be different from the shared home path '{0}'.",
                shared.path().display()
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_shared_entries_that_exist_as_real_files() {
        let shared = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let shadow = root.path().join("shadow");
        write(&shared.path().join("config.toml"), "model = \"gpt-5-codex\"\n");
        write(&shadow.join("config.toml"), "model = \"local\"\n");
        let error = materialize_codex_shadow_home(&resolve_codex_home_layout(&settings(shared.path(), Some(&shadow)))).unwrap_err();
        let CodexShadowHomeError::EntryConflict {
            entry_name,
            link_path,
            target_path,
            ..
        } = &error
        else {
            panic!("entry conflict expected")
        };
        assert_eq!(entry_name, "config.toml");
        assert_eq!(link_path, &shadow.join("config.toml"));
        assert_eq!(target_path, &shared.path().join("config.toml"));
        assert_eq!(
            error.to_string(),
            format!(
                "Cannot create Codex shadow home entry 'config.toml' because '{}' already exists and is not a symlink.",
                shadow.join("config.toml").display()
            )
        );
    }

    #[test]
    fn keeps_the_filesystem_operation_and_paths() {
        let shared_root = tempfile::tempdir().unwrap();
        let shared = shared_root.path().join("shared-home");
        let root = tempfile::tempdir().unwrap();
        let shadow = root.path().join("shadow");
        write(&shared, "not a directory\n");
        let error = materialize_codex_shadow_home(&resolve_codex_home_layout(&settings(&shared, Some(&shadow)))).unwrap_err();
        let CodexShadowHomeError::FileSystem { operation, path, .. } = &error else {
            panic!("filesystem error expected")
        };
        assert_eq!(*operation, "makeDirectory");
        assert!(path.starts_with(&shared));
        assert_eq!(
            error.to_string(),
            format!("Codex shadow home filesystem operation 'makeDirectory' failed for '{}'.", path.display())
        );
    }
}
