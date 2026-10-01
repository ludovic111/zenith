//! The file-backed secret store (`apps/server/src/auth/ServerSecretStore.ts`).
//!
//! Secrets are raw bytes in `<stateDir>/secrets/<name>.bin`. The directory is chmod 0700 when the
//! store opens, every file 0600. The TS server and the CLI (another process) share these files,
//! so creation is race-safe: `create` uses `O_CREAT|O_EXCL`, and `get_or_create_random` falls
//! back to reading the winner's file when it loses the race. Existing files written by the TS
//! server (e.g. `server-signing-key.bin`, which signs 30-day session cookies) are read unchanged.

use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use tokio::io::AsyncWriteExt;

use crate::defect::Defect;

/// `SecretStoreError`: the TS tagged errors, same messages.
#[derive(Debug, thiserror::Error)]
pub enum SecretStoreError {
    #[error("Failed to secure {resource}.")]
    Secure {
        resource: String,
        #[source]
        cause: io::Error,
    },
    #[error("Failed to read {resource}.")]
    Read {
        resource: String,
        #[source]
        cause: io::Error,
    },
    #[error("Failed to create temporary path for {resource}.")]
    TemporaryPath {
        resource: String,
        #[source]
        cause: io::Error,
    },
    #[error("Failed to persist {resource}.")]
    Persist {
        resource: String,
        #[source]
        cause: io::Error,
    },
    #[error("Failed to generate random bytes for {resource}.")]
    RandomGeneration {
        resource: String,
        #[source]
        cause: io::Error,
    },
    #[error("Failed to read {resource} after concurrent creation.")]
    ConcurrentRead { resource: String },
    #[error("Failed to remove {resource}.")]
    Remove {
        resource: String,
        #[source]
        cause: io::Error,
    },
    #[error("Failed to decode {resource}.")]
    Decode { resource: String, cause: Defect },
    #[error("Failed to encode {resource}.")]
    Encode { resource: String, cause: Defect },
}

impl SecretStoreError {
    /// The TS `_tag`.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Secure { .. } => "SecretStoreSecureError",
            Self::Read { .. } => "SecretStoreReadError",
            Self::TemporaryPath { .. } => "SecretStoreTemporaryPathError",
            Self::Persist { .. } => "SecretStorePersistError",
            Self::RandomGeneration { .. } => "SecretStoreRandomGenerationError",
            Self::ConcurrentRead { .. } => "SecretStoreConcurrentReadError",
            Self::Remove { .. } => "SecretStoreRemoveError",
            Self::Decode { .. } => "SecretStoreDecodeError",
            Self::Encode { .. } => "SecretStoreEncodeError",
        }
    }

    /// `isSecretAlreadyExistsError`: the failure was an `O_EXCL` collision.
    pub fn is_already_exists(&self) -> bool {
        matches!(self, Self::Persist { cause, .. } if cause.kind() == ErrorKind::AlreadyExists)
    }
}

/// The `ServerSecretStore` service.
#[derive(Debug, Clone)]
pub struct ServerSecretStore {
    directory: PathBuf,
}

impl ServerSecretStore {
    /// Open the store: create `secrets_dir` (recursively) and chmod it 0700.
    pub async fn open(secrets_dir: impl Into<PathBuf>) -> Result<Self, SecretStoreError> {
        let directory = secrets_dir.into();
        let resource = format!("secrets directory {}", directory.display());
        tokio::fs::create_dir_all(&directory).await.map_err(|cause| SecretStoreError::Secure {
            resource: resource.clone(),
            cause,
        })?;
        set_mode(&directory, 0o700)
            .await
            .map_err(|cause| SecretStoreError::Secure { resource, cause })?;
        Ok(Self { directory })
    }

    /// The directory, exposed for cross-process credential leases (`directory`).
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// `<dir>/<name>.bin`.
    pub fn secret_path(&self, name: &str) -> PathBuf {
        self.directory.join(format!("{name}.bin"))
    }

    /// The secret's bytes, or `None` when it does not exist.
    pub async fn get(&self, name: &str) -> Result<Option<Vec<u8>>, SecretStoreError> {
        match tokio::fs::read(self.secret_path(name)).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(cause) if cause.kind() == ErrorKind::NotFound => Ok(None),
            Err(cause) => Err(SecretStoreError::Read {
                resource: format!("secret {name}"),
                cause,
            }),
        }
    }

    /// Replace the secret: write `<path>.<uuid>.tmp` (0600), rename it into place, chmod 0600.
    /// The temp file is removed on failure.
    pub async fn set(&self, name: &str, value: &[u8]) -> Result<(), SecretStoreError> {
        let secret_path = self.secret_path(name);
        let mut temp_name = secret_path.clone().into_os_string();
        temp_name.push(format!(".{}.tmp", crate::ids::uuid_v4()));
        let temp_path = PathBuf::from(temp_name);
        let result = async {
            tokio::fs::write(&temp_path, value).await?;
            set_mode(&temp_path, 0o600).await?;
            tokio::fs::rename(&temp_path, &secret_path).await?;
            set_mode(&secret_path, 0o600).await
        }
        .await;
        if let Err(cause) = result {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(SecretStoreError::Persist {
                resource: format!("secret {name}"),
                cause,
            });
        }
        Ok(())
    }

    /// Create the secret only if it does not exist. Fails with an "already exists"
    /// [`SecretStoreError::Persist`] otherwise.
    ///
    /// TS opens the final path with `"wx"` and then writes, so a racing reader can briefly see an
    /// empty file. Here the bytes are written and fsynced to a private temp file (0600) first and
    /// published with `link(2)`, which is just as exclusive (`EEXIST`) but makes the file appear
    /// complete. The on-disk result is identical.
    pub async fn create(&self, name: &str, value: &[u8]) -> Result<(), SecretStoreError> {
        let secret_path = self.secret_path(name);
        let mut temp_name = secret_path.clone().into_os_string();
        temp_name.push(format!(".{}.tmp", crate::ids::uuid_v4()));
        let temp_path = PathBuf::from(temp_name);
        let result = async {
            let mut options = tokio::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&temp_path).await?;
            file.write_all(value).await?;
            file.sync_all().await?;
            drop(file);
            set_mode(&temp_path, 0o600).await?;
            tokio::fs::hard_link(&temp_path, &secret_path).await?;
            set_mode(&secret_path, 0o600).await
        }
        .await;
        let _ = tokio::fs::remove_file(&temp_path).await;
        result.map_err(|cause| SecretStoreError::Persist {
            resource: format!("secret {name}"),
            cause,
        })
    }

    /// The secret, creating it with `bytes` random bytes if absent. Concurrent creators (in this
    /// process or another) all end up with the same value: the loser of the `O_EXCL` race reads
    /// the winner's file.
    pub async fn get_or_create_random(&self, name: &str, bytes: usize) -> Result<Vec<u8>, SecretStoreError> {
        if let Some(existing) = self.get(name).await? {
            return Ok(existing);
        }
        let generated = crate::ids::random_bytes(bytes);
        match self.create(name, &generated).await {
            Ok(()) => Ok(generated),
            Err(error) if error.is_already_exists() => match self.get(name).await? {
                Some(winner) => Ok(winner),
                None => Err(SecretStoreError::ConcurrentRead {
                    resource: format!("secret {name}"),
                }),
            },
            Err(error) => Err(error),
        }
    }

    /// Delete the secret; a missing file is not an error.
    pub async fn remove(&self, name: &str) -> Result<(), SecretStoreError> {
        match tokio::fs::remove_file(self.secret_path(name)).await {
            Ok(()) => Ok(()),
            Err(cause) if cause.kind() == ErrorKind::NotFound => Ok(()),
            Err(cause) => Err(SecretStoreError::Remove {
                resource: format!("secret {name}"),
                cause,
            }),
        }
    }
}

#[cfg(unix)]
async fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await
}

#[cfg(not(unix))]
async fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[tokio::test]
    async fn get_set_remove_round_trip_with_private_modes() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = dir.path().join("userdata/secrets");
        let store = ServerSecretStore::open(&secrets).await.unwrap();
        assert_eq!(mode(&secrets), 0o700);
        assert_eq!(store.get("server-signing-key").await.unwrap(), None);

        store.set("server-signing-key", b"\x00\x01binary").await.unwrap();
        let path = secrets.join("server-signing-key.bin");
        assert_eq!(std::fs::read(&path).unwrap(), b"\x00\x01binary");
        assert_eq!(mode(&path), 0o600);
        assert_eq!(store.get("server-signing-key").await.unwrap().as_deref(), Some(&b"\x00\x01binary"[..]));

        store.set("server-signing-key", b"next").await.unwrap();
        assert_eq!(store.get("server-signing-key").await.unwrap().as_deref(), Some(&b"next"[..]));
        assert_eq!(std::fs::read_dir(&secrets).unwrap().count(), 1, "no temp files left");

        store.remove("server-signing-key").await.unwrap();
        store.remove("server-signing-key").await.unwrap();
        assert_eq!(store.get("server-signing-key").await.unwrap(), None);
    }

    #[tokio::test]
    async fn reopening_tightens_an_open_directory() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = dir.path().join("secrets");
        std::fs::create_dir_all(&secrets).unwrap();
        std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o755)).unwrap();
        ServerSecretStore::open(&secrets).await.unwrap();
        assert_eq!(mode(&secrets), 0o700);
    }

    #[tokio::test]
    async fn create_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let store = ServerSecretStore::open(dir.path().join("secrets")).await.unwrap();
        store.create("k", b"first").await.unwrap();
        let error = store.create("k", b"second").await.unwrap_err();
        assert!(error.is_already_exists());
        assert_eq!(error.tag(), "SecretStorePersistError");
        assert_eq!(error.to_string(), "Failed to persist secret k.");
        assert_eq!(store.get("k").await.unwrap().as_deref(), Some(&b"first"[..]));
        assert_eq!(mode(&store.secret_path("k")), 0o600);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_creators_agree_on_one_value() {
        for round in 0..20 {
            let dir = tempfile::tempdir().unwrap();
            let secrets = dir.path().join("secrets");
            let mut tasks = Vec::new();
            for _ in 0..16 {
                // Separate store instances model separate processes sharing the directory.
                let secrets = secrets.clone();
                tasks.push(tokio::spawn(async move {
                    let store = ServerSecretStore::open(&secrets).await.unwrap();
                    store.get_or_create_random("server-signing-key", 32).await.unwrap()
                }));
            }
            let mut values = Vec::new();
            for task in tasks {
                values.push(task.await.unwrap());
            }
            let on_disk = std::fs::read(secrets.join("server-signing-key.bin")).unwrap();
            assert_eq!(on_disk.len(), 32, "round {round}");
            assert!(values.iter().all(|value| *value == on_disk), "round {round}: every creator sees the winner");
        }
    }

    #[tokio::test]
    async fn existing_secret_is_returned_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = dir.path().join("secrets");
        std::fs::create_dir_all(&secrets).unwrap();
        std::fs::write(secrets.join("asset-access-signing-key.bin"), b"from-ts").unwrap();
        let store = ServerSecretStore::open(&secrets).await.unwrap();
        assert_eq!(store.get_or_create_random("asset-access-signing-key", 32).await.unwrap(), b"from-ts");
    }
}
