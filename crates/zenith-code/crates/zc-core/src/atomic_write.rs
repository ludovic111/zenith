//! Atomic file replacement (`apps/server/src/atomicWrite.ts`).
//!
//! The TS version creates a private temp directory *next to the target* (`<dir>/<name>.XXXXXX`,
//! Node `mkdtemp`), writes `contents.tmp` inside it, renames that over the target, and always
//! removes the temp directory afterwards. Writing next to the target keeps the rename on one file
//! system, so readers (the dashboard reads `settings.json` and `server-runtime.json` directly)
//! see either the old or the new file, never a partial one.

use std::io;
use std::path::{Path, PathBuf};

use rand::distr::{Alphanumeric, SampleString};

/// Atomically replace `file_path` with `contents` (UTF-8 text).
pub async fn write_file_string_atomically(file_path: &Path, contents: &str) -> io::Result<()> {
    write_file_atomically(file_path, contents.as_bytes()).await
}

/// Atomically replace `file_path` with `contents`.
pub async fn write_file_atomically(file_path: &Path, contents: &[u8]) -> io::Result<()> {
    let target_directory = parent_dir(file_path);
    tokio::fs::create_dir_all(&target_directory).await?;
    let prefix = format!("{}.", file_path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default());
    let temp_directory = make_temp_directory(&target_directory, &prefix).await?;
    let result = async {
        let temp_path = temp_directory.join("contents.tmp");
        tokio::fs::write(&temp_path, contents).await?;
        tokio::fs::rename(&temp_path, file_path).await
    }
    .await;
    // The scoped temp directory is removed whatever happened, like Effect's scope finalizer.
    let _ = tokio::fs::remove_dir_all(&temp_directory).await;
    result
}

/// Blocking variant of [`write_file_atomically`], for startup code and CLI paths that run before
/// (or outside) the async runtime.
pub fn write_file_atomically_blocking(file_path: &Path, contents: &[u8]) -> io::Result<()> {
    let target_directory = parent_dir(file_path);
    std::fs::create_dir_all(&target_directory)?;
    let prefix = format!("{}.", file_path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default());
    let temp_directory = make_temp_directory_blocking(&target_directory, &prefix)?;
    let result = (|| {
        let temp_path = temp_directory.join("contents.tmp");
        std::fs::write(&temp_path, contents)?;
        std::fs::rename(&temp_path, file_path)
    })();
    let _ = std::fs::remove_dir_all(&temp_directory);
    result
}

fn parent_dir(file_path: &Path) -> PathBuf {
    match file_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Node `fs.mkdtemp(join(directory, prefix))`: `prefix` plus six random characters, created
/// exclusively (mode 0700).
pub async fn make_temp_directory(directory: &Path, prefix: &str) -> io::Result<PathBuf> {
    for _ in 0..100 {
        let candidate = directory.join(format!("{prefix}{}", random_suffix()));
        let mut builder = tokio::fs::DirBuilder::new();
        #[cfg(unix)]
        builder.mode(0o700);
        match builder.create(&candidate).await {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "could not create a unique temp directory"))
}

fn make_temp_directory_blocking(directory: &Path, prefix: &str) -> io::Result<PathBuf> {
    for _ in 0..100 {
        let candidate = directory.join(format!("{prefix}{}", random_suffix()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        match builder.create(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "could not create a unique temp directory"))
}

fn random_suffix() -> String {
    Alphanumeric.sample_string(&mut rand::rng(), 6)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replaces_the_file_and_leaves_no_temp_directory() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested/settings.json");
        write_file_string_atomically(&target, "{\"a\":1}\n").await.unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{\"a\":1}\n");
        write_file_string_atomically(&target, "{}\n").await.unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{}\n");
        let leftovers: Vec<_> = std::fs::read_dir(target.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("settings.json")]);
    }

    #[tokio::test]
    async fn concurrent_writers_never_expose_a_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("state.json");
        let payloads: Vec<String> = (0..16).map(|i| format!("{}\n", "x".repeat(10_000 + i))).collect();
        let mut tasks = Vec::new();
        for payload in payloads.clone() {
            let target = target.clone();
            tasks.push(tokio::spawn(async move {
                write_file_string_atomically(&target, &payload).await.unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let final_contents = std::fs::read_to_string(&target).unwrap();
        assert!(payloads.contains(&final_contents));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn blocking_variant_writes_too() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.txt");
        write_file_atomically_blocking(&target, b"hello").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"hello");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn failure_cleans_up_the_temp_directory() {
        let dir = tempfile::tempdir().unwrap();
        // The target is an existing non-empty directory, so the rename fails.
        let target = dir.path().join("occupied");
        std::fs::create_dir_all(target.join("child")).unwrap();
        assert!(write_file_string_atomically(&target, "x").await.is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
