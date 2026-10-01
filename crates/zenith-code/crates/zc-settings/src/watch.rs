//! Directory watching with a debounce: `fs.watch(dir).pipe(Stream.filter(…),
//! Stream.debounce(100 ms))` followed by `Stream.runForEach`.
//!
//! Editors emit several events per save (truncate, write, rename) and the watch can fire before
//! the content is flushed, so a change is acted on only after 100 ms without events. Handlers
//! run one at a time; events arriving while one runs start the next quiet period.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use notify::{RecursiveMode, Watcher};
use tokio::sync::mpsc;

/// The debounce every watched directory uses.
pub const WATCH_DEBOUNCE: Duration = Duration::from_millis(100);

/// What runs after a quiet period.
pub type ChangeHandler = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// A live watch; dropping it stops watching.
pub struct DirWatch {
    _watcher: notify::RecommendedWatcher,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for DirWatch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl std::fmt::Debug for DirWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirWatch").finish_non_exhaustive()
    }
}

/// Watch `dir` (not recursively). `filter` sees each event path; `on_change` runs after
/// `debounce` without accepted events. Must be called inside a tokio runtime.
pub fn watch_directory(
    dir: &Path,
    filter: impl Fn(&Path) -> bool + Send + Sync + 'static,
    debounce: Duration,
    on_change: ChangeHandler,
) -> notify::Result<DirWatch> {
    let (sender, mut receiver) = mpsc::unbounded_channel::<()>();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else {
            return;
        };
        if event.kind.is_access() {
            return;
        }
        if event.paths.is_empty() || event.paths.iter().any(|path| filter(path)) {
            let _ = sender.send(());
        }
    })?;
    watcher.watch(dir, RecursiveMode::NonRecursive)?;
    let task = tokio::spawn(async move {
        while receiver.recv().await.is_some() {
            loop {
                match tokio::time::timeout(debounce, receiver.recv()).await {
                    Ok(Some(())) => continue,
                    Ok(None) => return,
                    Err(_) => break,
                }
            }
            on_change().await;
        }
    });
    Ok(DirWatch { _watcher: watcher, task })
}

/// The settings-file filter of `serverSettings.ts` / `keybindings.ts`: the event names the
/// file (by base name, by path, or resolved against the directory).
pub fn file_filter(target: &Path) -> impl Fn(&Path) -> bool + Send + Sync + 'static {
    let file_name = target.file_name().map(|name| name.to_os_string());
    let resolved = zc_core::paths::resolve_path(target);
    let canonical: Option<PathBuf> = target
        .parent()
        .and_then(|parent| std::fs::canonicalize(parent).ok())
        .zip(file_name.clone())
        .map(|(parent, name)| parent.join(name));
    move |path: &Path| path.file_name().map(|name| name.to_os_string()) == file_name || path == resolved || canonical.as_deref() == Some(path)
}
