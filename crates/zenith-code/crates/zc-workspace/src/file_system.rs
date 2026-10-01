//! `WorkspaceFileSystem` (`workspace/WorkspaceFileSystem.ts`): `projects.readFile` and
//! `projects.writeFile`.
//!
//! - Reads: a workspace-relative path must stay inside the root after resolving symlinks
//!   (`realpath` of both); an absolute path reads any host file in place (deliberate: clients
//!   show files an agent wrote elsewhere). The file is opened non-blocking (a FIFO cannot hang
//!   the open; it is then refused as not a file), at most 1 MiB is read (`truncated` when the
//!   file is larger), a NUL byte marks it binary, and the bytes are decoded as UTF-8 the way
//!   `TextDecoder` does (BOM dropped, invalid sequences replaced).
//! - Writes: workspace-relative only; parents are created; then the workspace's search indexes
//!   are refreshed.
//!
//! Deviation (hardening): the TS write follows symlinks, so a symlinked directory or file inside
//! the workspace could make it write outside the root. Here a write whose existing parent
//! directory, or whose existing target, resolves outside the real root is refused with
//! `WorkspaceFilePathEscapeError` (`resolved_path_outside_root`), the same rule reads apply.

use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;

use zc_contracts::{ProjectReadFileResult, ProjectWriteFileResult};

use crate::entries::WorkspaceEntries;
use crate::errors::{node_fs_defect, platform_error_defect, FileOperation, FileSystemError};
use crate::paths::{self, WorkspacePaths};

pub const PROJECT_READ_FILE_MAX_BYTES: u64 = 1024 * 1024;

/// The `WorkspaceFileSystem` service.
#[derive(Clone, Debug)]
pub struct WorkspaceFileSystem {
    paths: WorkspacePaths,
    entries: WorkspaceEntries,
}

struct ReadTarget {
    relative_path: String,
    real_target_path: String,
}

fn realpath(path: &str) -> std::io::Result<String> {
    std::fs::canonicalize(path).map(|real| real.to_string_lossy().into_owned())
}

fn escapes(root: &str, target: &str) -> bool {
    let relative = paths::relative(root, target);
    relative.starts_with("../") || relative == ".." || paths::is_absolute(&relative)
}

/// `new TextDecoder("utf-8").decode(bytes)`: a leading BOM is dropped, invalid sequences become
/// U+FFFD (maximal subparts, like `from_utf8_lossy`).
pub fn decode_utf8_like_text_decoder(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

impl WorkspaceFileSystem {
    pub fn new(paths: WorkspacePaths, entries: WorkspaceEntries) -> Self {
        Self { paths, entries }
    }

    fn operation_error(
        cwd: &str,
        relative_path: &str,
        resolved_path: &str,
        operation_path: &str,
        operation: FileOperation,
        cause: zc_core::Defect,
    ) -> FileSystemError {
        FileSystemError::Operation {
            workspace_root: cwd.to_owned(),
            relative_path: relative_path.to_owned(),
            resolved_path: resolved_path.to_owned(),
            operation_path: operation_path.to_owned(),
            operation,
            cause,
        }
    }

    fn resolve_read_target(&self, cwd: &str, relative_path: &str) -> Result<ReadTarget, FileSystemError> {
        let requested = relative_path.trim();
        if paths::is_absolute(requested) {
            let real_target_path = realpath(requested).map_err(|error| {
                Self::operation_error(
                    cwd,
                    relative_path,
                    requested,
                    requested,
                    FileOperation::RealpathTarget,
                    node_fs_defect(&error, "realpath", requested),
                )
            })?;
            return Ok(ReadTarget {
                relative_path: requested.to_owned(),
                real_target_path,
            });
        }

        let target = self.paths.resolve_relative_path_within_root(cwd, relative_path)?;
        let real_root = realpath(cwd).map_err(|error| {
            Self::operation_error(
                cwd,
                relative_path,
                &target.absolute_path,
                cwd,
                FileOperation::RealpathWorkspaceRoot,
                node_fs_defect(&error, "realpath", cwd),
            )
        })?;
        let real_target_path = realpath(&target.absolute_path).map_err(|error| {
            Self::operation_error(
                cwd,
                relative_path,
                &target.absolute_path,
                &target.absolute_path,
                FileOperation::RealpathTarget,
                node_fs_defect(&error, "realpath", &target.absolute_path),
            )
        })?;
        if escapes(&real_root, &real_target_path) {
            return Err(FileSystemError::PathEscape {
                workspace_root: cwd.to_owned(),
                relative_path: relative_path.to_owned(),
                resolved_workspace_root: real_root,
                resolved_path: real_target_path,
            });
        }
        Ok(ReadTarget {
            relative_path: target.relative_path,
            real_target_path,
        })
    }

    fn read_file_blocking(&self, cwd: &str, relative_path: &str) -> Result<ProjectReadFileResult, FileSystemError> {
        let target = self.resolve_read_target(cwd, relative_path)?;
        let real = target.real_target_path.as_str();
        let op_error = |operation: FileOperation, syscall: &str, error: std::io::Error| {
            Self::operation_error(cwd, relative_path, real, real, operation, node_fs_defect(&error, syscall, real))
        };
        // Non-blocking so a FIFO cannot hang the open; the stat below rejects it.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(real)
            .map_err(|error| op_error(FileOperation::Open, "open", error))?;
        let metadata = file.metadata().map_err(|error| op_error(FileOperation::Stat, "fstat", error))?;
        if !metadata.is_file() {
            return Err(FileSystemError::NotFile {
                workspace_root: cwd.to_owned(),
                relative_path: relative_path.to_owned(),
                resolved_path: real.to_owned(),
            });
        }
        let size = metadata.len();
        let bytes_to_read = size.min(PROJECT_READ_FILE_MAX_BYTES) as usize;
        let mut buffer = Vec::with_capacity(bytes_to_read);
        file.take(bytes_to_read as u64)
            .read_to_end(&mut buffer)
            .map_err(|error| op_error(FileOperation::Read, "read", error))?;
        if buffer.contains(&0) {
            return Err(FileSystemError::Binary {
                workspace_root: cwd.to_owned(),
                relative_path: relative_path.to_owned(),
                resolved_path: real.to_owned(),
            });
        }
        Ok(ProjectReadFileResult {
            relative_path: target.relative_path,
            contents: decode_utf8_like_text_decoder(&buffer),
            byte_length: size as i64,
            truncated: size > PROJECT_READ_FILE_MAX_BYTES,
        })
    }

    /// `readFile({ cwd, relativePath })`.
    pub async fn read_file(&self, cwd: &str, relative_path: &str) -> Result<ProjectReadFileResult, FileSystemError> {
        let this = self.clone();
        let (cwd, relative_path) = (cwd.to_owned(), relative_path.to_owned());
        tokio::task::spawn_blocking(move || this.read_file_blocking(&cwd, &relative_path))
            .await
            .expect("readFile task panicked")
    }

    /// The write-side symlink check (see the module docs).
    fn check_write_containment(&self, cwd: &str, relative_path: &str, absolute_path: &str) -> Result<(), FileSystemError> {
        // A root that does not exist yet has no symlinks in it; `mkdir -p` creates it.
        let Ok(real_root) = realpath(cwd) else { return Ok(()) };
        let escape = |resolved_path: String| FileSystemError::PathEscape {
            workspace_root: cwd.to_owned(),
            relative_path: relative_path.to_owned(),
            resolved_workspace_root: real_root.clone(),
            resolved_path,
        };
        let mut ancestor = paths::dirname(absolute_path);
        loop {
            if let Ok(real) = realpath(&ancestor) {
                if escapes(&real_root, &real) {
                    return Err(escape(real));
                }
                break;
            }
            let parent = paths::dirname(&ancestor);
            if parent == ancestor {
                break;
            }
            ancestor = parent;
        }
        if let Ok(metadata) = std::fs::symlink_metadata(absolute_path) {
            if metadata.file_type().is_symlink() {
                match realpath(absolute_path) {
                    Ok(real) if !escapes(&real_root, &real) => {}
                    Ok(real) => return Err(escape(real)),
                    // A dangling link would create its target wherever it points.
                    Err(_) => {
                        let link = std::fs::read_link(absolute_path)
                            .map(|link| paths::resolve(&paths::dirname(absolute_path), &link.to_string_lossy()))
                            .unwrap_or_else(|_| absolute_path.to_owned());
                        return Err(escape(link));
                    }
                }
            }
        }
        Ok(())
    }

    fn write_file_blocking(&self, cwd: &str, relative_path: &str, contents: &str) -> Result<String, FileSystemError> {
        let target = self.paths.resolve_relative_path_within_root(cwd, relative_path)?;
        self.check_write_containment(cwd, relative_path, &target.absolute_path)?;
        let parent = paths::dirname(&target.absolute_path);
        std::fs::create_dir_all(&parent).map_err(|error| {
            Self::operation_error(
                cwd,
                relative_path,
                &target.absolute_path,
                &parent,
                FileOperation::MakeDirectory,
                platform_error_defect(&error, "makeDirectory", "mkdir", &parent),
            )
        })?;
        std::fs::write(&target.absolute_path, contents).map_err(|error| {
            Self::operation_error(
                cwd,
                relative_path,
                &target.absolute_path,
                &target.absolute_path,
                FileOperation::WriteFile,
                platform_error_defect(&error, "writeFile", "open", &target.absolute_path),
            )
        })?;
        Ok(target.relative_path)
    }

    /// `writeFile({ cwd, relativePath, contents })`.
    pub async fn write_file(&self, cwd: &str, relative_path: &str, contents: &str) -> Result<ProjectWriteFileResult, FileSystemError> {
        let this = self.clone();
        let (owned_cwd, owned_path, owned_contents) = (cwd.to_owned(), relative_path.to_owned(), contents.to_owned());
        let relative_path = tokio::task::spawn_blocking(move || this.write_file_blocking(&owned_cwd, &owned_path, &owned_contents))
            .await
            .expect("writeFile task panicked")?;
        self.entries.refresh(cwd).await;
        Ok(ProjectWriteFileResult { relative_path })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_decoder_semantics() {
        assert_eq!(decode_utf8_like_text_decoder(b"\xEF\xBB\xBFhi"), "hi");
        assert_eq!(decode_utf8_like_text_decoder(b"a\xFFb"), "a\u{FFFD}b");
        assert_eq!(decode_utf8_like_text_decoder("é".as_bytes()), "é");
    }
}
