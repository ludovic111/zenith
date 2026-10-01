//! Port of `Layers/ProviderSessionReaper.ts`: every 5 minutes, stop provider sessions idle for
//! 30 minutes, unless their thread still has an active turn or live background work.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::directory::ProviderSessionDirectory;
use crate::hooks::ThreadShells;
use crate::service::ProviderServiceImpl;

pub const DEFAULT_INACTIVITY_THRESHOLD_MS: i64 = 30 * 60 * 1000;
pub const DEFAULT_SWEEP_INTERVAL_MS: i64 = 5 * 60 * 1000;

/// `ProviderSessionReaperLiveOptions`.
#[derive(Clone)]
pub struct ReaperOptions {
    pub inactivity_threshold_ms: i64,
    pub sweep_interval_ms: i64,
    /// Epoch milliseconds (tests pin it).
    pub clock: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl Default for ReaperOptions {
    fn default() -> Self {
        Self {
            inactivity_threshold_ms: DEFAULT_INACTIVITY_THRESHOLD_MS,
            sweep_interval_ms: DEFAULT_SWEEP_INTERVAL_MS,
            clock: Arc::new(zc_core::now_millis),
        }
    }
}

/// `ProviderSessionReaper`.
#[derive(Clone)]
pub struct ProviderSessionReaper {
    service: ProviderServiceImpl,
    directory: ProviderSessionDirectory,
    thread_shells: Option<Arc<dyn ThreadShells>>,
    options: ReaperOptions,
}

impl ProviderSessionReaper {
    pub fn new(
        service: ProviderServiceImpl,
        directory: ProviderSessionDirectory,
        thread_shells: Option<Arc<dyn ThreadShells>>,
        options: ReaperOptions,
    ) -> Self {
        let mut options = options;
        options.inactivity_threshold_ms = options.inactivity_threshold_ms.max(1);
        options.sweep_interval_ms = options.sweep_interval_ms.max(1);
        Self {
            service,
            directory,
            thread_shells,
            options,
        }
    }

    /// One sweep; returns how many sessions it stopped.
    pub async fn sweep(&self) -> Result<usize, crate::ProviderServiceError> {
        // Stopped rows stay for their resume cursors and far outnumber live ones.
        let bindings = self.directory.list_bindings(true).await?;
        let now = (self.options.clock)();
        let threshold = self.options.inactivity_threshold_ms;
        let mut reaped = 0;
        for with in &bindings {
            let binding = &with.binding;
            let Some(last_seen_ms) = zc_core::time::parse_iso_millis(&with.last_seen_at) else {
                tracing::warn!(thread_id = %binding.thread_id, provider = %binding.provider, last_seen_at = %with.last_seen_at, "provider.session.reaper.invalid-last-seen");
                continue;
            };
            if now - last_seen_ms < threshold {
                continue;
            }
            let thread = match &self.thread_shells {
                Some(shells) => shells
                    .get_thread_shell(&binding.thread_id)
                    .await
                    .map_err(|detail| crate::ProviderServiceError::persistence("ProviderSessionReaper.sweep", detail))?,
                None => None,
            };
            // Ingestion bumps the session's updatedAt when a turn settles: long turns get a
            // full idle window after that.
            let session_updated_ms = thread
                .as_ref()
                .and_then(|thread| thread.session_updated_at.as_deref())
                .map(|updated_at| zc_core::time::parse_iso_millis(updated_at).unwrap_or(i64::MIN))
                .unwrap_or(last_seen_ms);
            let last_activity_ms = last_seen_ms.max(session_updated_ms);
            let idle_ms = now - last_activity_ms;
            if idle_ms < threshold {
                continue;
            }
            if let Some(active_turn_id) = thread.as_ref().and_then(|thread| thread.session_active_turn_id.as_ref()) {
                tracing::debug!(thread_id = %binding.thread_id, %active_turn_id, idle_ms, "provider.session.reaper.skipped-active-turn");
                continue;
            }
            if let Some(liveness) = thread.as_ref().and_then(|thread| thread.background_liveness.as_ref()) {
                tracing::debug!(thread_id = %binding.thread_id, %liveness, idle_ms, "provider.session.reaper.skipped-background-work");
                continue;
            }
            match self.service.stop_session(&binding.thread_id).await {
                Ok(()) => {
                    tracing::info!(thread_id = %binding.thread_id, provider = %binding.provider, idle_ms, reason = "inactivity_threshold", "provider.session.reaped");
                    reaped += 1;
                }
                Err(error) => {
                    tracing::warn!(thread_id = %binding.thread_id, provider = %binding.provider, idle_ms, %error, "provider.session.reaper.stop-failed")
                }
            }
        }
        if reaped > 0 {
            tracing::info!(reaped, live_bindings = bindings.len(), "provider.session.reaper.sweep-complete");
        }
        Ok(reaped)
    }

    /// `start()`: sweep now, then every `sweep_interval_ms`, until `stop` is cancelled.
    pub fn start(&self, stop: CancellationToken) -> tokio::task::JoinHandle<()> {
        let reaper = self.clone();
        tracing::info!(
            inactivity_threshold_ms = self.options.inactivity_threshold_ms,
            sweep_interval_ms = self.options.sweep_interval_ms,
            "provider.session.reaper.started"
        );
        tokio::spawn(async move {
            loop {
                let sweep = tokio::spawn({
                    let reaper = reaper.clone();
                    async move { reaper.sweep().await }
                });
                match sweep.await {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => tracing::warn!(%error, "provider.session.reaper.sweep-failed"),
                    Err(defect) => tracing::warn!(%defect, "provider.session.reaper.sweep-defect"),
                }
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(reaper.options.sweep_interval_ms as u64)) => {}
                }
            }
        })
    }
}
