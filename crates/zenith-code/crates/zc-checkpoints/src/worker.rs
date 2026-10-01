//! `makeDrainableWorker` (`packages/shared/src/DrainableWorker.ts`): a queue processed one item
//! at a time, in order, with `drain` resolving once the queue is empty and the worker idle.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::{mpsc, Notify};

type Process<T> = Arc<dyn Fn(T) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

struct State {
    /// Enqueued items not yet fully processed.
    outstanding: AtomicUsize,
    idle: Notify,
}

/// A sequential worker. Cloning shares it; the task stops when every clone is dropped and the
/// queue is empty.
pub struct DrainableWorker<T> {
    queue: mpsc::UnboundedSender<T>,
    state: Arc<State>,
}

impl<T> Clone for DrainableWorker<T> {
    fn clone(&self) -> Self {
        Self {
            queue: self.queue.clone(),
            state: self.state.clone(),
        }
    }
}

impl<T: Send + 'static> DrainableWorker<T> {
    /// Starts the worker task (needs a Tokio runtime). `process` must not panic; failures are
    /// the callback's to log, like the TS workers' `catchCause`.
    pub fn new<F, Fut>(process: F) -> Self
    where
        F: Fn(T) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let process: Process<T> = Arc::new(move |item| Box::pin(process(item)));
        let (queue, mut items) = mpsc::unbounded_channel::<T>();
        let state = Arc::new(State {
            outstanding: AtomicUsize::new(0),
            idle: Notify::new(),
        });
        let worker_state = state.clone();
        tokio::spawn(async move {
            while let Some(item) = items.recv().await {
                // A panicking item must not wedge `drain`: run it as its own task.
                let run = tokio::spawn(process(item));
                if let Err(error) = run.await {
                    tracing::warn!(error = %error, "drainable worker item panicked");
                }
                if worker_state.outstanding.fetch_sub(1, Ordering::SeqCst) == 1 {
                    worker_state.idle.notify_waiters();
                }
            }
        });
        Self { queue, state }
    }

    /// `enqueue(item)`.
    pub fn enqueue(&self, item: T) {
        self.state.outstanding.fetch_add(1, Ordering::SeqCst);
        if self.queue.send(item).is_err() && self.state.outstanding.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.state.idle.notify_waiters();
        }
    }

    /// `drain`: waits until everything enqueued so far (and anything enqueued meanwhile) is
    /// processed.
    pub async fn drain(&self) {
        loop {
            let notified = self.state.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.state.outstanding.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    #[tokio::test]
    async fn processes_in_order_and_drains() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let worker = DrainableWorker::new(move |item: u32| {
            let sink = sink.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(u64::from(5 - item))).await;
                sink.lock().unwrap().push(item);
            }
        });
        for item in 0..5 {
            worker.enqueue(item);
        }
        worker.drain().await;
        assert_eq!(*seen.lock().unwrap(), vec![0, 1, 2, 3, 4]);
        worker.drain().await;
    }

    #[tokio::test]
    async fn a_panicking_item_does_not_wedge_drain() {
        let worker = DrainableWorker::new(|item: u32| async move {
            assert_ne!(item, 1, "boom");
        });
        worker.enqueue(1);
        worker.enqueue(2);
        worker.drain().await;
    }
}
