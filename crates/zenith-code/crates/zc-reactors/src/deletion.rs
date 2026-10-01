//! `orchestration/Layers/ThreadDeletionReactor.ts`: on `thread.deleted`, stop the thread's
//! provider session and close its terminals (deleting their history). Cleanup failures are
//! logged at debug level and swallowed.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::StreamExt;
use serde_json::json;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use zc_contracts::{OrchestrationEvent, ThreadId};
use zc_ports::contracts::{ProviderStopSessionInput, TerminalCloseInput};
use zc_ports::{OrchestrationDispatch, ProviderService, TerminalManager};

use crate::common::pretty;
use crate::runtime::DrainableWorker;

/// `ThreadDeletionReactorService`.
pub struct ThreadDeletionReactor {
    engine: Arc<dyn OrchestrationDispatch>,
    worker: DrainableWorker<ThreadId>,
    /// The highest sequence the subscriber has handed to the worker.
    seen_sequence: Arc<watch::Sender<i64>>,
    /// Whether [`Self::start`] subscribed (until then nothing moves the watermark).
    started: AtomicBool,
    stop: CancellationToken,
}

impl ThreadDeletionReactor {
    pub fn new(
        engine: Arc<dyn OrchestrationDispatch>,
        providers: Arc<dyn ProviderService>,
        terminals: Arc<dyn TerminalManager>,
        stop: CancellationToken,
    ) -> Self {
        let worker = DrainableWorker::start(stop.clone(), move |thread_id: ThreadId| {
            let providers = providers.clone();
            let terminals = terminals.clone();
            async move {
                if let Err(error) = providers.stop_session(ProviderStopSessionInput(json!({"threadId": thread_id}))).await {
                    tracing::debug!(thread_id = %thread_id, cause = %pretty(&error), "thread deletion cleanup skipped provider session stop");
                }
                if let Err(error) = terminals.close(TerminalCloseInput(json!({"threadId": thread_id, "deleteHistory": true}))).await {
                    tracing::debug!(thread_id = %thread_id, cause = %pretty(&error), "thread deletion cleanup skipped terminal close");
                }
            }
        });
        Self {
            engine,
            worker,
            seen_sequence: Arc::new(watch::channel(0).0),
            started: AtomicBool::new(false),
            stop,
        }
    }

    /// `start()`: subscribes to domain events (events before the subscription are not
    /// replayed, so the watermark starts at the current head).
    pub async fn start(&self) {
        let mut events = self.engine.subscribe_domain_events();
        let head = self.engine.latest_sequence().await;
        self.seen_sequence.send_modify(|seen| *seen = (*seen).max(head));
        self.started.store(true, Ordering::SeqCst);
        let worker = self.worker.clone();
        let seen = self.seen_sequence.clone();
        let stop = self.stop.clone();
        tokio::spawn(async move {
            loop {
                let event = tokio::select! {
                    _ = stop.cancelled() => break,
                    event = events.next() => event,
                };
                let Some(event) = event else { break };
                let sequence = event_sequence(&event);
                if let OrchestrationEvent::ThreadDeleted(deleted) = &event {
                    worker.enqueue(deleted.payload.thread_id.clone());
                }
                seen.send_modify(|seen| *seen = (*seen).max(sequence));
            }
        });
    }

    /// `drainThrough(target)`: waits until the subscriber has seen `target`, then for the
    /// worker. Waiting through a `thread.created` sequence covers every deletion before it.
    ///
    /// Without a running subscriber (before [`Self::start`], e.g. a server built but not
    /// started, or after [`Self::stop`] at shutdown) no deletion can be pending and nothing
    /// would ever move the watermark, so it returns at once instead of waiting forever.
    pub async fn drain_through(&self, target: i64) {
        if !self.started.load(Ordering::SeqCst) || self.stop.is_cancelled() {
            return;
        }
        let mut receiver = self.seen_sequence.subscribe();
        tokio::select! {
            _ = receiver.wait_for(|seen| *seen >= target) => {}
            _ = self.stop.cancelled() => return,
        }
        self.worker.drain().await;
    }

    pub fn stop(&self) {
        self.stop.cancel();
    }
}

fn event_sequence(event: &OrchestrationEvent) -> i64 {
    zc_orchestration::OrchestrationEventExt::sequence(event)
}
