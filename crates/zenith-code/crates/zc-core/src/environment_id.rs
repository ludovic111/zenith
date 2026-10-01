//! The environment id file (`ServerEnvironmentIdentity` in
//! `apps/server/src/environment/ServerEnvironment.ts`).
//!
//! `<stateDir>/environment-id` holds one UUID plus a newline. It identifies this server to its
//! clients forever (the dashboard also reads it for thread URLs), so creation must never replace
//! an id another process already published. The protocol, kept exactly:
//!
//! 1. Read the file; a non-empty trimmed value wins.
//! 2. Otherwise write a fresh UUID to a private temp file and `link(2)` it to the final path
//!    (`EEXIST` is fine: someone else won), then read again.
//! 3. If the file is still missing or empty (an empty file left behind by a crash), *recover*:
//!    link the candidate to `environment-id.recovery` (first recoverer wins), copy that over a
//!    temp file and `rename(2)` it onto `environment-id`, so delayed initializers publish the
//!    same winner. Then read again.
//! 4. Still nothing: [`EnvironmentIdError::Initialize`].

use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use rand::RngCore;

/// `ServerEnvironmentIdPersistenceError`.
#[derive(Debug, thiserror::Error)]
pub enum EnvironmentIdError {
    #[error("Server environment ID {operation} failed at '{}'.", .path.display())]
    Io {
        /// `"check" | "read" | "write"`.
        operation: &'static str,
        path: PathBuf,
        #[source]
        cause: io::Error,
    },
    #[error("Server environment ID file is missing or empty after initialization at '{}'.", .path.display())]
    Initialize { path: PathBuf },
}

/// Read the environment id, creating it race-safely when absent.
pub async fn read_or_create_environment_id(state_dir: &Path, environment_id_path: &Path) -> Result<String, EnvironmentIdError> {
    if let Some(existing) = read_persisted(environment_id_path).await? {
        return Ok(existing);
    }
    let generated = crate::ids::uuid_v4();
    persist(state_dir, environment_id_path, &generated, Mode::Create).await?;
    if let Some(winner) = read_persisted(environment_id_path).await? {
        return Ok(winner);
    }
    persist(state_dir, environment_id_path, &generated, Mode::Recover).await?;
    match read_persisted(environment_id_path).await? {
        Some(winner) => Ok(winner),
        None => Err(EnvironmentIdError::Initialize {
            path: environment_id_path.to_path_buf(),
        }),
    }
}

/// Read the id without creating it: `None` when the file is missing or blank.
pub async fn read_environment_id(environment_id_path: &Path) -> Result<Option<String>, EnvironmentIdError> {
    read_persisted(environment_id_path).await
}

async fn read_persisted(path: &Path) -> Result<Option<String>, EnvironmentIdError> {
    match tokio::fs::try_exists(path).await {
        Ok(false) => return Ok(None),
        Ok(true) => {}
        Err(cause) => {
            return Err(EnvironmentIdError::Io {
                operation: "check",
                path: path.to_path_buf(),
                cause,
            })
        }
    }
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            let trimmed = text.trim();
            Ok((!trimmed.is_empty()).then(|| trimmed.to_owned()))
        }
        // Removed between the check and the read: same as missing.
        Err(cause) if cause.kind() == ErrorKind::NotFound => Ok(None),
        Err(cause) => Err(EnvironmentIdError::Io {
            operation: "read",
            path: path.to_path_buf(),
            cause,
        }),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Create,
    Recover,
}

async fn persist(state_dir: &Path, environment_id_path: &Path, value: &str, mode: Mode) -> Result<(), EnvironmentIdError> {
    let write_error = |cause: io::Error| EnvironmentIdError::Io {
        operation: "write",
        path: environment_id_path.to_path_buf(),
        cause,
    };
    // Effect `makeTempFileScoped({ directory: stateDir, prefix: ".environment-id-" })`:
    // `<stateDir>/.environment-id-XXXXXX/<12 hex>`, the directory removed afterwards.
    let temp_dir = crate::atomic_write::make_temp_directory(state_dir, ".environment-id-")
        .await
        .map_err(write_error)?;
    let result = persist_in(&temp_dir, environment_id_path, value, mode).await;
    let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    result.map_err(write_error)
}

async fn persist_in(temp_dir: &Path, environment_id_path: &Path, value: &str, mode: Mode) -> io::Result<()> {
    let mut random = [0u8; 6];
    rand::rng().fill_bytes(&mut random);
    let temp_path = temp_dir.join(random.iter().map(|b| format!("{b:02x}")).collect::<String>());
    let destination = match mode {
        Mode::Create => environment_id_path.to_path_buf(),
        Mode::Recover => {
            let mut name = environment_id_path.as_os_str().to_owned();
            name.push(".recovery");
            PathBuf::from(name)
        }
    };
    tokio::fs::write(&temp_path, format!("{value}\n")).await?;
    // Publish the completed file without replacing an id created by another process.
    match tokio::fs::hard_link(&temp_path, &destination).await {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    if mode == Mode::Recover {
        // Keep the recovery id so delayed initializers also publish the same winner.
        tokio::fs::remove_file(&temp_path).await?;
        tokio::fs::copy(&destination, &temp_path).await?;
        tokio::fs::rename(&temp_path, environment_id_path).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn creates_once_and_then_reads_the_same_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("environment-id");
        let first = read_or_create_environment_id(dir.path(), &path).await.unwrap();
        assert!(crate::ids::is_canonical_uuid(&first));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{first}\n"));
        let second = read_or_create_environment_id(dir.path(), &path).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "temp files are cleaned up");
    }

    #[tokio::test]
    async fn keeps_an_existing_id_written_by_ts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("environment-id");
        std::fs::write(&path, "  2f0c5f0e-1111-4222-8333-444455556666\n").unwrap();
        assert_eq!(
            read_or_create_environment_id(dir.path(), &path).await.unwrap(),
            "2f0c5f0e-1111-4222-8333-444455556666"
        );
    }

    #[tokio::test]
    async fn recovers_from_an_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("environment-id");
        std::fs::write(&path, "").unwrap();
        let id = read_or_create_environment_id(dir.path(), &path).await.unwrap();
        assert!(crate::ids::is_canonical_uuid(&id));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{id}\n"));
        let recovery = dir.path().join("environment-id.recovery");
        assert_eq!(std::fs::read_to_string(recovery).unwrap(), format!("{id}\n"));
        // A later initializer that also finds nothing usable publishes the same winner.
        std::fs::write(&path, "\n").unwrap();
        assert_eq!(read_or_create_environment_id(dir.path(), &path).await.unwrap(), id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_initializers_agree() {
        for _ in 0..20 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("environment-id");
            let tasks: Vec<_> = (0..12)
                .map(|_| {
                    let state = dir.path().to_path_buf();
                    let path = path.clone();
                    tokio::spawn(async move { read_or_create_environment_id(&state, &path).await.unwrap() })
                })
                .collect();
            let mut ids = Vec::new();
            for task in tasks {
                ids.push(task.await.unwrap());
            }
            assert!(ids.windows(2).all(|pair| pair[0] == pair[1]), "{ids:?}");
        }
    }

    #[tokio::test]
    async fn read_without_create() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("environment-id");
        assert_eq!(read_environment_id(&path).await.unwrap(), None);
    }
}
