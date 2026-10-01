//! The boundary between the engine and the SQL projection pipeline
//! (`orchestration/Services/ProjectionPipeline.ts`), which WP-09 implements.
//!
//! The engine calls [`ProjectionPipeline::project_event_deferred`] once per appended event,
//! **inside its own dispatch transaction** and on the database writer thread (the `&Conn` it
//! passes is that transaction). As in `Layers/ProjectionPipeline.ts`:
//!
//! - the implementation runs the nine projectors for the event in one nested transaction of
//!   its own (`conn.transaction(..)`, which is a `SAVEPOINT` there), then advances every
//!   projector cursor in `projection_state` to the event (`upsertMany`);
//! - an error rolls back that savepoint and is returned; the engine then rolls back the whole
//!   dispatch (event appends included) and reconciles;
//! - attachment files are **not** deleted there: the implementation returns a
//!   [`DeferredCleanup`] that the engine runs only after its transaction commits, before it
//!   publishes the events. Most events have none ([`DeferredCleanup::none`]).
//!
//! [`ProjectionPipeline::bootstrap`] runs once before the engine loads its command read model:
//! it replays each projector from its cursor and retries pending attachment cleanup
//! (`bootstrap` in TS), in transactions of its own.

use async_trait::async_trait;
use zc_contracts::OrchestrationEvent;
use zc_db::{Conn, DbError};

/// Work to run after the dispatch transaction commits (`Effect<void>` returned by
/// `projectEventDeferred`). It cannot fail; it logs what it could not do.
#[derive(Default)]
pub struct DeferredCleanup(Option<Box<dyn FnOnce() + Send + 'static>>);

impl DeferredCleanup {
    /// Nothing to do (`Effect.void`).
    pub fn none() -> Self {
        Self(None)
    }

    pub fn new(cleanup: impl FnOnce() + Send + 'static) -> Self {
        Self(Some(Box::new(cleanup)))
    }

    pub fn is_none(&self) -> bool {
        self.0.is_none()
    }

    /// Runs the cleanup (blocking work: file deletions).
    pub fn run(self) {
        if let Some(cleanup) = self.0 {
            cleanup();
        }
    }
}

impl std::fmt::Debug for DeferredCleanup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() { "DeferredCleanup(some)" } else { "DeferredCleanup(none)" })
    }
}

/// `OrchestrationProjectionPipelineShape`, as the engine uses it.
#[async_trait]
pub trait ProjectionPipeline: Send + Sync {
    /// `bootstrap`: resume every projector from its stored cursor.
    async fn bootstrap(&self) -> Result<(), DbError>;

    /// `projectEventDeferred(event)`: project one persisted event inside the caller's
    /// transaction and return its post-commit attachment cleanup.
    fn project_event_deferred(&self, conn: &Conn, event: &OrchestrationEvent) -> Result<DeferredCleanup, DbError>;
}

/// A pipeline that projects nothing: the engine alone (tests, replays, and until WP-09 lands).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopProjectionPipeline;

#[async_trait]
impl ProjectionPipeline for NoopProjectionPipeline {
    async fn bootstrap(&self) -> Result<(), DbError> {
        Ok(())
    }

    fn project_event_deferred(&self, _conn: &Conn, _event: &OrchestrationEvent) -> Result<DeferredCleanup, DbError> {
        Ok(DeferredCleanup::none())
    }
}
