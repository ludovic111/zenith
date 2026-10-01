//! Port of `workspace/WorkspaceFileSystem.test.ts`, plus the path-containment and symlink
//! security cases for reads, writes and listings.

use std::os::unix::fs::symlink;
use std::path::Path;
use std::sync::Arc;

use zc_contracts::ProjectReadFileResult;
use zc_core::VcsProcess;
use zc_workspace::errors::{FileOperation, FileSystemError, TaggedError};
use zc_workspace::{FffFactory, SearchIndexMap, WorkspaceEntries, WorkspaceFileSystem, WorkspacePaths};

fn services() -> (WorkspaceEntries, WorkspaceFileSystem) {
    let entries = WorkspaceEntries::new(
        WorkspacePaths::new(),
        SearchIndexMap::new(Arc::new(FffFactory)),
        Arc::new(VcsProcess::default()),
    );
    let file_system = WorkspaceFileSystem::new(WorkspacePaths::new(), entries.clone());
    (entries, file_system)
}

fn file_system() -> WorkspaceFileSystem {
    services().1
}

fn temp_dir() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("zc-ws-files-").tempdir().unwrap()
}

fn path_of(dir: &tempfile::TempDir) -> String {
    dir.path().to_string_lossy().into_owned()
}

fn write(cwd: &str, relative: &str, contents: &[u8]) {
    let path = Path::new(cwd).join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn realpath(path: &str) -> String {
    std::fs::canonicalize(path).unwrap().to_string_lossy().into_owned()
}

// ------------------------------------------------------------------------------------------
// readFile
// ------------------------------------------------------------------------------------------

#[tokio::test]
async fn reads_utf8_files_relative_to_the_workspace_root() {
    let dir = temp_dir();
    let cwd = path_of(&dir);
    write(&cwd, "src/index.ts", b"export const answer = 42;\n");
    let result = file_system().read_file(&cwd, "src/index.ts").await.unwrap();
    assert_eq!(
        result,
        ProjectReadFileResult {
            relative_path: "src/index.ts".into(),
            contents: "export const answer = 42;\n".into(),
            byte_length: 26,
            truncated: false,
        }
    );
}

#[tokio::test]
async fn reads_host_files_outside_the_workspace_root_by_absolute_path() {
    let dir = temp_dir();
    let outside = temp_dir();
    let cwd = path_of(&dir);
    write(&path_of(&outside), "cleanup-report.md", b"# Report\n");
    let absolute = format!("{}/cleanup-report.md", path_of(&outside));
    let result = file_system().read_file(&cwd, &absolute).await.unwrap();
    assert_eq!(
        result,
        ProjectReadFileResult {
            relative_path: absolute,
            contents: "# Report\n".into(),
            byte_length: 9,
            truncated: false
        }
    );
}

#[tokio::test]
async fn rejects_a_fifo_without_blocking_on_open() {
    let dir = temp_dir();
    let outside = temp_dir();
    let fifo = format!("{}/pipe", path_of(&outside));
    let status = std::process::Command::new("mkfifo").arg(&fifo).status().unwrap();
    assert!(status.success());
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), file_system().read_file(&path_of(&dir), &fifo))
        .await
        .expect("the open must not block")
        .unwrap_err();
    assert!(matches!(error, FileSystemError::NotFile { .. }), "{error:?}");
}

#[tokio::test]
async fn rejects_reads_outside_the_workspace_root() {
    let dir = temp_dir();
    let error = file_system().read_file(&path_of(&dir), "../escape.md").await.unwrap_err();
    assert!(error
        .message()
        .contains("Workspace file path must be relative to the project root: ../escape.md"));
    assert!(matches!(error, FileSystemError::OutsideRoot(_)));
}

#[tokio::test]
async fn rejects_symlinks_that_resolve_outside_the_workspace_root() {
    let dir = temp_dir();
    let outside = temp_dir();
    let cwd = path_of(&dir);
    write(&path_of(&outside), "secret.txt", b"outside\n");
    symlink(outside.path().join("secret.txt"), Path::new(&cwd).join("linked-secret.txt")).unwrap();
    let error = file_system().read_file(&cwd, "linked-secret.txt").await.unwrap_err();
    assert_eq!(
        error,
        FileSystemError::PathEscape {
            workspace_root: cwd.clone(),
            relative_path: "linked-secret.txt".into(),
            resolved_workspace_root: realpath(&cwd),
            resolved_path: realpath(&format!("{}/secret.txt", path_of(&outside))),
        }
    );
    assert!(error.cause().is_none());
}

#[tokio::test]
async fn rejects_files_reached_through_a_symlinked_directory_that_leaves_the_root() {
    let dir = temp_dir();
    let outside = temp_dir();
    let cwd = path_of(&dir);
    write(&path_of(&outside), "nested/secret.txt", b"outside\n");
    symlink(outside.path(), Path::new(&cwd).join("external")).unwrap();
    let error = file_system().read_file(&cwd, "external/nested/secret.txt").await.unwrap_err();
    assert!(matches!(error, FileSystemError::PathEscape { .. }), "{error:?}");
    // `..` segments that stay inside the root are fine; ones that leave it are not.
    write(&cwd, "src/a.txt", b"a");
    assert_eq!(file_system().read_file(&cwd, "src/../src/a.txt").await.unwrap().relative_path, "src/a.txt");
    let escaping = file_system().read_file(&cwd, "src/../../x").await.unwrap_err();
    assert!(matches!(escaping, FileSystemError::OutsideRoot(_)));
}

#[tokio::test]
async fn reads_through_symlinks_that_stay_inside_the_root() {
    let dir = temp_dir();
    let cwd = path_of(&dir);
    write(&cwd, "docs/real.md", b"inside\n");
    symlink(Path::new(&cwd).join("docs/real.md"), Path::new(&cwd).join("alias.md")).unwrap();
    let result = file_system().read_file(&cwd, "alias.md").await.unwrap();
    assert_eq!((result.relative_path.as_str(), result.contents.as_str()), ("alias.md", "inside\n"));
}

#[tokio::test]
async fn works_when_the_root_itself_is_reached_through_a_symlink() {
    let dir = temp_dir();
    let real_root = format!("{}/real-root", path_of(&dir));
    write(&real_root, "a.txt", b"a");
    let link = format!("{}/link-root", path_of(&dir));
    symlink(&real_root, &link).unwrap();
    let result = file_system().read_file(&link, "a.txt").await.unwrap();
    assert_eq!(result.contents, "a");
}

#[tokio::test]
async fn rejects_directories_without_manufacturing_an_io_cause() {
    let dir = temp_dir();
    let cwd = path_of(&dir);
    std::fs::create_dir(Path::new(&cwd).join("src")).unwrap();
    let error = file_system().read_file(&cwd, "src").await.unwrap_err();
    assert_eq!(
        error,
        FileSystemError::NotFile {
            workspace_root: cwd.clone(),
            relative_path: "src".into(),
            resolved_path: realpath(&format!("{cwd}/src")),
        }
    );
    assert!(error.cause().is_none());
}

#[tokio::test]
async fn rejects_binary_files_without_leaking_their_contents_into_the_error() {
    let dir = temp_dir();
    let cwd = path_of(&dir);
    write(&cwd, "asset.bin", &[0x61, 0, 0x62]);
    let error = file_system().read_file(&cwd, "asset.bin").await.unwrap_err();
    assert_eq!(
        error,
        FileSystemError::Binary {
            workspace_root: cwd.clone(),
            relative_path: "asset.bin".into(),
            resolved_path: realpath(&format!("{cwd}/asset.bin")),
        }
    );
    assert!(error.cause().is_none());
}

#[tokio::test]
async fn preserves_the_real_cause_and_path_for_io_failures() {
    let dir = temp_dir();
    let cwd = path_of(&dir);
    let resolved = format!("{cwd}/missing.txt");
    let error = file_system().read_file(&cwd, "missing.txt").await.unwrap_err();
    match &error {
        FileSystemError::Operation {
            workspace_root,
            relative_path,
            resolved_path,
            operation_path,
            operation,
            cause,
        } => {
            assert_eq!(workspace_root, &cwd);
            assert_eq!(relative_path, "missing.txt");
            assert_eq!(resolved_path, &resolved);
            assert_eq!(operation_path, &resolved);
            assert_eq!(*operation, FileOperation::RealpathTarget);
            assert_eq!(cause.message(), format!("ENOENT: no such file or directory, realpath '{resolved}'"));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn caps_reads_at_one_mebibyte_and_flags_truncation() {
    let dir = temp_dir();
    let cwd = path_of(&dir);
    let big = vec![b'x'; 1024 * 1024 + 10];
    write(&cwd, "big.txt", &big);
    let result = file_system().read_file(&cwd, "big.txt").await.unwrap();
    assert_eq!(result.contents.len(), 1024 * 1024);
    assert_eq!(result.byte_length, 1024 * 1024 + 10);
    assert!(result.truncated);
}

// ------------------------------------------------------------------------------------------
// writeFile
// ------------------------------------------------------------------------------------------

#[tokio::test]
async fn writes_files_relative_to_the_workspace_root() {
    let dir = temp_dir();
    let cwd = path_of(&dir);
    let result = file_system().write_file(&cwd, "plans/effect-rpc.md", "# Plan\n").await.unwrap();
    assert_eq!(result.relative_path, "plans/effect-rpc.md");
    assert_eq!(std::fs::read_to_string(format!("{cwd}/plans/effect-rpc.md")).unwrap(), "# Plan\n");
}

#[tokio::test]
async fn rejects_writes_by_absolute_path() {
    let dir = temp_dir();
    let outside = temp_dir();
    let absolute = format!("{}/cleanup-report.md", path_of(&outside));
    let error = file_system().write_file(&path_of(&dir), &absolute, "# Edited\n").await.unwrap_err();
    assert!(matches!(error, FileSystemError::OutsideRoot(_)));
    assert!(!Path::new(&absolute).exists());
}

#[tokio::test]
async fn invalidates_workspace_entry_search_cache_after_writes() {
    let dir = temp_dir();
    let cwd = path_of(&dir);
    write(&cwd, "src/existing.ts", b"export {};\n");
    let (entries, file_system) = services();
    let before = entries.list(&cwd, None).await.unwrap();
    assert!(!before.entries.iter().any(|entry| entry.path == "plans/effect-rpc.md"));
    file_system.write_file(&cwd, "plans/effect-rpc.md", "# Plan\n").await.unwrap();
    let after = entries.list(&cwd, None).await.unwrap();
    assert!(after.entries.iter().any(|entry| entry.path == "plans/effect-rpc.md"));
    assert!(!after.truncated);
}

#[tokio::test]
async fn rejects_writes_outside_the_workspace_root() {
    let parent = temp_dir();
    let cwd = format!("{}/project", path_of(&parent));
    std::fs::create_dir(&cwd).unwrap();
    let error = file_system().write_file(&cwd, "../escape.md", "# nope\n").await.unwrap_err();
    assert!(error
        .message()
        .contains("Workspace file path must be relative to the project root: ../escape.md"));
    assert!(!Path::new(&format!("{}/escape.md", path_of(&parent))).exists());
}

#[tokio::test]
async fn refuses_writes_through_a_symlinked_directory_that_leaves_the_root() {
    let dir = temp_dir();
    let outside = temp_dir();
    let cwd = path_of(&dir);
    symlink(outside.path(), Path::new(&cwd).join("external")).unwrap();
    let error = file_system().write_file(&cwd, "external/new/dir/file.md", "x").await.unwrap_err();
    assert!(matches!(error, FileSystemError::PathEscape { .. }), "{error:?}");
    assert!(!outside.path().join("new").exists(), "no directory may be created outside the root");
}

#[tokio::test]
async fn refuses_writes_through_a_symlinked_file_that_leaves_the_root() {
    let dir = temp_dir();
    let outside = temp_dir();
    let cwd = path_of(&dir);
    write(&path_of(&outside), "target.md", b"original");
    symlink(outside.path().join("target.md"), Path::new(&cwd).join("link.md")).unwrap();
    let error = file_system().write_file(&cwd, "link.md", "overwritten").await.unwrap_err();
    assert!(matches!(error, FileSystemError::PathEscape { .. }), "{error:?}");
    assert_eq!(std::fs::read_to_string(outside.path().join("target.md")).unwrap(), "original");
    // A dangling link would create its target outside: refused too.
    symlink(outside.path().join("absent.md"), Path::new(&cwd).join("dangling.md")).unwrap();
    let error = file_system().write_file(&cwd, "dangling.md", "x").await.unwrap_err();
    assert!(matches!(error, FileSystemError::PathEscape { .. }), "{error:?}");
    assert!(!outside.path().join("absent.md").exists());
}

#[tokio::test]
async fn writes_through_symlinks_that_stay_inside_the_root() {
    let dir = temp_dir();
    let cwd = path_of(&dir);
    write(&cwd, "docs/real.md", b"old");
    symlink(Path::new(&cwd).join("docs"), Path::new(&cwd).join("alias")).unwrap();
    file_system().write_file(&cwd, "alias/real.md", "new").await.unwrap();
    assert_eq!(std::fs::read_to_string(Path::new(&cwd).join("docs/real.md")).unwrap(), "new");
}

#[tokio::test]
async fn creates_a_missing_root_like_mkdir_p() {
    let dir = temp_dir();
    let cwd = format!("{}/not-yet/project", path_of(&dir));
    let result = file_system().write_file(&cwd, "a/b.md", "x").await.unwrap();
    assert_eq!(result.relative_path, "a/b.md");
    assert_eq!(std::fs::read_to_string(format!("{cwd}/a/b.md")).unwrap(), "x");
}
