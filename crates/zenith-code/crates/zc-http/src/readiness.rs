//! The command readiness gate (`server.ts` `commandReadinessLayer`, plan §6.18 step 11):
//! every HTTP request, the `/ws` upgrade included, waits until startup has finished. If
//! startup fails, waiting requests fail with 500.

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use tokio::sync::watch;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Readiness {
    Starting,
    Ready,
    Failed(String),
}

/// Cheap to clone; all clones share the state.
#[derive(Clone, Debug)]
pub struct ReadinessGate {
    tx: watch::Sender<Readiness>,
}

impl Default for ReadinessGate {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadinessGate {
    pub fn new() -> Self {
        Self {
            tx: watch::Sender::new(Readiness::Starting),
        }
    }

    /// A gate that is already open (tests, dev servers).
    pub fn ready() -> Self {
        let gate = Self::new();
        gate.mark_ready();
        gate
    }

    pub fn mark_ready(&self) {
        self.tx.send_replace(Readiness::Ready);
    }

    pub fn mark_failed(&self, reason: impl Into<String>) {
        self.tx.send_replace(Readiness::Failed(reason.into()));
    }

    pub fn state(&self) -> Readiness {
        self.tx.borrow().clone()
    }

    /// Waits for the end of startup.
    pub async fn wait(&self) -> Result<(), String> {
        let mut rx = self.tx.subscribe();
        let state = rx
            .wait_for(|s| !matches!(s, Readiness::Starting))
            .await
            .map(|s| s.clone())
            .unwrap_or(Readiness::Failed("server stopped".into()));
        match state {
            Readiness::Failed(reason) => Err(reason),
            _ => Ok(()),
        }
    }
}

/// The middleware: `axum::middleware::from_fn_with_state(gate, readiness)`.
pub async fn readiness(State(gate): State<ReadinessGate>, request: Request, next: Next) -> Response {
    match gate.wait().await {
        Ok(()) => next.run(request).await,
        Err(reason) => {
            tracing::warn!("request refused, startup failed: {reason}");
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn waits_then_opens() {
        let gate = ReadinessGate::new();
        let waiter = {
            let gate = gate.clone();
            tokio::spawn(async move { gate.wait().await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        gate.mark_ready();
        assert_eq!(waiter.await.unwrap(), Ok(()));
        assert_eq!(gate.wait().await, Ok(()));
    }

    #[tokio::test]
    async fn failure_fails_waiters() {
        let gate = ReadinessGate::new();
        gate.mark_failed("db");
        assert_eq!(gate.wait().await, Err("db".into()));
    }
}
