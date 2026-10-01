//! The SQL projection pipeline as the orchestration engine runs it: each appended event is
//! projected inside the dispatch transaction, and its attachment cleanup runs once the
//! transaction has committed (the engine runs it on a blocking task, off the writer thread,
//! so it can go back through the database actor).

use async_trait::async_trait;
use zc_contracts::OrchestrationEvent;
use zc_db::{Conn, Db, DbError};
use zc_orchestration::pipeline::DeferredCleanup;

use crate::ProjectionPipeline;

pub struct EnginePipeline {
    pub pipeline: ProjectionPipeline,
    pub db: Db,
}

#[async_trait]
impl zc_orchestration::pipeline::ProjectionPipeline for EnginePipeline {
    async fn bootstrap(&self) -> Result<(), DbError> {
        self.pipeline.bootstrap_on(&self.db).await
    }

    fn project_event_deferred(&self, conn: &Conn, event: &OrchestrationEvent) -> Result<DeferredCleanup, DbError> {
        let cleanup = self.pipeline.project_event_deferred(conn, event)?;
        if cleanup.is_empty() {
            return Ok(DeferredCleanup::none());
        }
        let db = self.db.clone();
        Ok(DeferredCleanup::new(move || {
            if let Err(error) = db.call_blocking(move |conn| Ok(cleanup.run(conn))) {
                tracing::warn!(%error, "attachment cleanup after commit");
            }
        }))
    }
}
