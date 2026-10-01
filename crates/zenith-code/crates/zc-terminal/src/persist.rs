//! Terminal history on disk (`terminal/Manager.ts`: `historyPath`, `readHistory`,
//! `readHistoryTail`, `deleteHistory`, `deleteAllHistoryForThread`, and the debounced
//! `persistWorker` built on `KeyedCoalescingWorker`).
//!
//! Files live in `<stateDir>/logs/terminals/`:
//!
//! - `terminal_<b64url(threadId)>.log` for the default terminal (`term-1`),
//! - `terminal_<b64url(threadId)>_<b64url(terminalId)>.log` for the others,
//! - `<threadId with [^a-zA-Z0-9._-] → _>.log`, the legacy name of the default terminal,
//!   migrated to the new name on first open.
//!
//! Only the last `byte_limit` bytes of a file are read (from a character boundary), then
//! re-capped by the line limit; a file that was longer is rewritten with what was kept.
//! Writes are coalesced per terminal: a write waits 40 ms (`DEFAULT_PERSIST_DEBOUNCE_MS`)
//! and then stores the history as it is at that moment; "immediate" writes skip the wait.
//! Unlike the TS worker (one fiber for every terminal, so a chatty terminal could delay the
//! others), each terminal has its own writer task; per terminal the behaviour is the same.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::Notify;
use zc_core::Defect;

use crate::contracts::{TerminalError, TerminalHistoryOperation, DEFAULT_TERMINAL_ID};
use crate::history::BoundedTerminalHistory;

/// `DEFAULT_PERSIST_DEBOUNCE_MS`.
pub const DEFAULT_PERSIST_DEBOUNCE: Duration = Duration::from_millis(40);

fn b64url(value: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.as_bytes())
}

/// `toSafeThreadId`.
pub fn safe_thread_id(thread_id: &str) -> String {
    format!("terminal_{}", b64url(thread_id))
}

/// `legacySafeThreadId`.
pub fn legacy_safe_thread_id(thread_id: &str) -> String {
    thread_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect()
}

/// Reads and deletes history files.
#[derive(Debug, Clone)]
pub struct HistoryStore {
    pub logs_dir: PathBuf,
    pub line_limit: usize,
    pub byte_limit: usize,
}

fn history_error(operation: TerminalHistoryOperation, thread_id: &str, terminal_id: &str, cause: &std::io::Error) -> TerminalError {
    TerminalError::TerminalHistoryError {
        operation,
        thread_id: thread_id.to_owned(),
        terminal_id: terminal_id.to_owned(),
        cause: Some(Defect::from(cause)),
    }
}

impl HistoryStore {
    /// `historyPath`.
    pub fn history_path(&self, thread_id: &str, terminal_id: &str) -> PathBuf {
        let thread_part = safe_thread_id(thread_id);
        if terminal_id == DEFAULT_TERMINAL_ID {
            self.logs_dir.join(format!("{thread_part}.log"))
        } else {
            self.logs_dir.join(format!("{thread_part}_{}.log", b64url(terminal_id)))
        }
    }

    /// `legacyHistoryPath`.
    pub fn legacy_history_path(&self, thread_id: &str) -> PathBuf {
        self.logs_dir.join(format!("{}.log", legacy_safe_thread_id(thread_id)))
    }

    fn empty(&self) -> BoundedTerminalHistory {
        BoundedTerminalHistory::new(self.line_limit, "", self.byte_limit)
    }

    /// `readHistoryTail`: the last `byte_limit` bytes, starting at a character boundary,
    /// decoded leniently (a BOM is kept). Also says whether the file was longer.
    pub async fn read_history_tail(&self, path: &Path) -> std::io::Result<(String, bool)> {
        let mut file = tokio::fs::File::open(path).await?;
        let size = file.metadata().await?.len();
        let limit = self.byte_limit as u64;
        let offset = size.saturating_sub(limit);
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut bytes = Vec::with_capacity((size - offset) as usize);
        file.take(size - offset).read_to_end(&mut bytes).await?;
        let mut start = 0;
        if offset > 0 {
            while start < bytes.len() && bytes[start] & 0xc0 == 0x80 {
                start += 1;
            }
        }
        Ok((String::from_utf8_lossy(&bytes[start..]).into_owned(), offset > 0))
    }

    /// `readHistory`: the stored history of a terminal (migrating the legacy file of the
    /// default terminal), capped to the limits.
    pub async fn read_history(&self, thread_id: &str, terminal_id: &str) -> Result<BoundedTerminalHistory, TerminalError> {
        let next_path = self.history_path(thread_id, terminal_id);
        let exists = tokio::fs::try_exists(&next_path)
            .await
            .map_err(|e| history_error(TerminalHistoryOperation::Read, thread_id, terminal_id, &e))?;
        if exists {
            let (raw, truncated) = self
                .read_history_tail(&next_path)
                .await
                .map_err(|e| history_error(TerminalHistoryOperation::Read, thread_id, terminal_id, &e))?;
            let mut history = BoundedTerminalHistory::new(self.line_limit, &raw, self.byte_limit);
            if truncated || history.value() != raw {
                tokio::fs::write(&next_path, history.value())
                    .await
                    .map_err(|e| history_error(TerminalHistoryOperation::Truncate, thread_id, terminal_id, &e))?;
            }
            return Ok(history);
        }

        if terminal_id != DEFAULT_TERMINAL_ID {
            return Ok(self.empty());
        }

        let legacy_path = self.legacy_history_path(thread_id);
        let legacy_exists = tokio::fs::try_exists(&legacy_path)
            .await
            .map_err(|e| history_error(TerminalHistoryOperation::Migrate, thread_id, terminal_id, &e))?;
        if !legacy_exists {
            return Ok(self.empty());
        }
        let (raw, _) = self
            .read_history_tail(&legacy_path)
            .await
            .map_err(|e| history_error(TerminalHistoryOperation::Migrate, thread_id, terminal_id, &e))?;
        let mut history = BoundedTerminalHistory::new(self.line_limit, &raw, self.byte_limit);
        tokio::fs::write(&next_path, history.value())
            .await
            .map_err(|e| history_error(TerminalHistoryOperation::Migrate, thread_id, terminal_id, &e))?;
        if let Err(error) = remove_file_force(&legacy_path).await {
            tracing::warn!(thread_id, %error, "failed to remove legacy terminal history");
        }
        Ok(history)
    }

    /// `deleteHistory`.
    pub async fn delete_history(&self, thread_id: &str, terminal_id: &str) {
        if let Err(error) = remove_file_force(&self.history_path(thread_id, terminal_id)).await {
            tracing::warn!(thread_id, terminal_id, %error, "failed to delete terminal history");
        }
        if terminal_id == DEFAULT_TERMINAL_ID {
            if let Err(error) = remove_file_force(&self.legacy_history_path(thread_id)).await {
                tracing::warn!(thread_id, terminal_id, %error, "failed to delete terminal history");
            }
        }
    }

    /// `deleteAllHistoryForThread`.
    pub async fn delete_all_history_for_thread(&self, thread_id: &str) {
        let safe = safe_thread_id(thread_id);
        let prefix = format!("{safe}_");
        let current = format!("{safe}.log");
        let legacy = format!("{}.log", legacy_safe_thread_id(thread_id));
        let Ok(mut entries) = tokio::fs::read_dir(&self.logs_dir).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == current || name == legacy || name.starts_with(&prefix) {
                if let Err(error) = remove_file_force(&entry.path()).await {
                    tracing::warn!(thread_id, %error, "failed to delete terminal histories for thread");
                }
            }
        }
    }
}

/// `fileSystem.remove(path, { force: true })`: a missing file is fine.
async fn remove_file_force(path: &Path) -> std::io::Result<()> {
    match tokio::fs::remove_file(path).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// What to write: the file, and how to read the history when the write happens.
#[derive(Clone)]
pub struct PersistRequest {
    pub path: PathBuf,
    pub source: Arc<dyn Fn() -> String + Send + Sync>,
    pub immediate: bool,
}

#[derive(Default)]
struct KeyState {
    pending: Option<PersistRequest>,
}

struct WorkerInner {
    keys: Mutex<HashMap<String, KeyState>>,
    idle: Notify,
    debounce: Duration,
}

/// The per-terminal coalescing writer (see the module docs).
#[derive(Clone)]
pub struct PersistWorker {
    inner: Arc<WorkerInner>,
}

impl PersistWorker {
    pub fn new(debounce: Duration) -> Self {
        Self {
            inner: Arc::new(WorkerInner {
                keys: Mutex::new(HashMap::new()),
                idle: Notify::new(),
                debounce,
            }),
        }
    }

    /// Queues a write; merges with one already queued for the key (latest source, immediate
    /// if either was).
    pub fn enqueue(&self, key: &str, request: PersistRequest) {
        let start = {
            let mut keys = self.inner.keys.lock().unwrap();
            match keys.get_mut(key) {
                Some(state) => {
                    let immediate = request.immediate || state.pending.as_ref().is_some_and(|pending| pending.immediate);
                    state.pending = Some(PersistRequest { immediate, ..request });
                    false
                }
                None => {
                    keys.insert(key.to_owned(), KeyState { pending: Some(request) });
                    true
                }
            }
        };
        if start {
            let inner = self.inner.clone();
            let key = key.to_owned();
            tokio::spawn(async move { run_key(inner, key).await });
        }
    }

    /// `drainKey`: waits until the key has nothing queued or in flight.
    pub async fn drain_key(&self, key: &str) {
        loop {
            let idle = self.inner.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if !self.inner.keys.lock().unwrap().contains_key(key) {
                return;
            }
            idle.await;
        }
    }

    /// Waits for every key.
    pub async fn drain_all(&self) {
        loop {
            let idle = self.inner.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if self.inner.keys.lock().unwrap().is_empty() {
                return;
            }
            idle.await;
        }
    }
}

async fn run_key(inner: Arc<WorkerInner>, key: String) {
    loop {
        let request = {
            let mut keys = inner.keys.lock().unwrap();
            match keys.get_mut(&key).and_then(|state| state.pending.take()) {
                Some(request) => request,
                None => {
                    keys.remove(&key);
                    drop(keys);
                    inner.idle.notify_waiters();
                    return;
                }
            }
        };
        if !request.immediate {
            tokio::time::sleep(inner.debounce).await;
        }
        let contents = (request.source)();
        if let Err(error) = tokio::fs::write(&request.path, contents).await {
            tracing::warn!(path = %request.path.display(), %error, "failed to persist terminal history");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names() {
        let store = HistoryStore {
            logs_dir: PathBuf::from("/logs"),
            line_limit: 5,
            byte_limit: 100,
        };
        assert_eq!(store.history_path("thread-1", "term-1"), PathBuf::from("/logs/terminal_dGhyZWFkLTE.log"));
        assert_eq!(
            store.history_path("thread-1", "sidecar"),
            PathBuf::from("/logs/terminal_dGhyZWFkLTE_c2lkZWNhcg.log")
        );
        assert_eq!(store.legacy_history_path("a/b c.d"), PathBuf::from("/logs/a_b_c.d.log"));
        assert_eq!(safe_thread_id("é?>~~"), "terminal_w6k_Pn5-");
    }

    #[tokio::test]
    async fn coalesces_and_drains() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.log");
        let worker = PersistWorker::new(Duration::from_millis(20));
        let value = Arc::new(Mutex::new("one".to_owned()));
        let source = {
            let value = value.clone();
            Arc::new(move || value.lock().unwrap().clone()) as Arc<dyn Fn() -> String + Send + Sync>
        };
        worker.enqueue(
            "k",
            PersistRequest {
                path: path.clone(),
                source: source.clone(),
                immediate: false,
            },
        );
        *value.lock().unwrap() = "two".into();
        worker.drain_key("k").await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        *value.lock().unwrap() = "three".into();
        worker.enqueue(
            "k",
            PersistRequest {
                path: path.clone(),
                source,
                immediate: true,
            },
        );
        worker.drain_key("k").await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "three");
    }
}
