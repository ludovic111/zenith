//! `withWorkspaceLease` (`apps/server/src/workspace/workspaceLease.ts`): one checkout at a
//! time. Terminal startup and worktree removal for the same resolved directory run one after
//! the other, whatever thread they belong to.
//!
//! The table is process-wide, like the TS module's. Every crate that starts in or removes a
//! checkout (terminal startup in zc-terminal, worktree deletion, zc-workspace) takes the lease
//! through this function so they exclude each other. It lived in zc-terminal first and moved
//! here so they all share one table.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

struct Lease {
    semaphore: Arc<tokio::sync::Mutex<()>>,
    users: usize,
}

fn leases() -> &'static Mutex<HashMap<PathBuf, Lease>> {
    static LEASES: OnceLock<Mutex<HashMap<PathBuf, Lease>>> = OnceLock::new();
    LEASES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Drops the lease's user count even if the future is cancelled.
struct UserGuard(PathBuf);

impl Drop for UserGuard {
    fn drop(&mut self) {
        let mut leases = leases().lock().unwrap();
        if let Some(lease) = leases.get_mut(&self.0) {
            lease.users -= 1;
            if lease.users == 0 {
                leases.remove(&self.0);
            }
        }
    }
}

/// Runs `work` while holding the lease of `cwd` (an already resolved path).
pub async fn with_workspace_lease<T>(cwd: &Path, work: impl Future<Output = T>) -> T {
    let semaphore = {
        let mut leases = leases().lock().unwrap();
        let lease = leases.entry(cwd.to_path_buf()).or_insert_with(|| Lease {
            semaphore: Arc::new(tokio::sync::Mutex::new(())),
            users: 0,
        });
        lease.users += 1;
        lease.semaphore.clone()
    };
    let _user = UserGuard(cwd.to_path_buf());
    let _permit = semaphore.lock().await;
    work.await
}

/// How many leases are held or awaited (for tests).
pub fn active_leases() -> usize {
    leases().lock().unwrap().len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn serializes_work_per_directory() {
        let path = PathBuf::from("/lease-test/serial");
        let running = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<_> = (0..4)
            .map(|_| {
                let (path, running, max) = (path.clone(), running.clone(), max.clone());
                tokio::spawn(async move {
                    with_workspace_lease(&path, async {
                        let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                        max.fetch_max(now, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        running.fetch_sub(1, Ordering::SeqCst);
                    })
                    .await
                })
            })
            .collect();
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(max.load(Ordering::SeqCst), 1);
        assert!(!leases().lock().unwrap().contains_key(&path));
    }
}
