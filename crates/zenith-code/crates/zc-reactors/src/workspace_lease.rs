//! `workspace/workspaceLease.ts`: a process-wide lock per resolved workspace path, so checkout
//! removal (storage cleanup) and session startup (the command reactor) never interleave on the
//! same directory.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

struct Lease {
    lock: Arc<tokio::sync::Mutex<()>>,
    users: usize,
}

static LEASES: LazyLock<Mutex<HashMap<String, Lease>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// `path.resolve(value)`: absolute, normalized, without touching the file system.
pub fn resolve_path(value: &str) -> String {
    let path = Path::new(value);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")).join(path)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    let text = out.to_string_lossy().into_owned();
    if text.len() > 1 {
        text.trim_end_matches('/').to_owned()
    } else {
        text
    }
}

/// `withWorkspaceLease(cwd, effect)`: runs `work` holding the lease of `cwd`.
pub async fn with_workspace_lease<F: Future>(cwd: &str, work: F) -> F::Output {
    let lock = {
        let mut leases = LEASES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let lease = leases.entry(cwd.to_owned()).or_insert_with(|| Lease {
            lock: Arc::new(tokio::sync::Mutex::new(())),
            users: 0,
        });
        lease.users += 1;
        lease.lock.clone()
    };
    let release = ReleaseOnDrop(cwd.to_owned());
    let output = {
        let _permit = lock.lock().await;
        work.await
    };
    drop(release);
    output
}

struct ReleaseOnDrop(String);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        let mut leases = LEASES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(lease) = leases.get_mut(&self.0) {
            lease.users -= 1;
            if lease.users == 0 {
                leases.remove(&self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn resolves_paths() {
        assert_eq!(resolve_path("/a/b/../c/./d/"), "/a/c/d");
        assert_eq!(resolve_path("/"), "/");
    }

    #[tokio::test]
    async fn serializes_work_on_one_path() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<_> = (0..4)
            .map(|_| {
                let active = active.clone();
                let peak = peak.clone();
                tokio::spawn(async move {
                    with_workspace_lease("/lease-test", async {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        tokio::task::yield_now().await;
                        active.fetch_sub(1, Ordering::SeqCst);
                    })
                    .await
                })
            })
            .collect();
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), 1);
        assert!(LEASES.lock().unwrap().get("/lease-test").is_none());
    }
}
