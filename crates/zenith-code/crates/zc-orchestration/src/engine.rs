//! `orchestration/Layers/OrchestrationEngine.ts`: the single serialized writer of the event
//! store.
//!
//! Commands go through one unbounded queue to one worker task, which owns the command read
//! model. Per command (`processEnvelope`):
//!
//! 1. the receipt of the command id answers a retry: same aggregate and `accepted` → the old
//!    sequence; `rejected` → `OrchestrationCommandPreviouslyRejectedError`; another aggregate →
//!    `OrchestrationCommandIdConflictError`;
//! 2. guards: `thread.auto-settle` is rejected when the thread has events after the command's
//!    snapshot or live background work, `thread.pull-request.sync` when the thread was
//!    recreated; `thread.meta.update` with a legacy link loads the project's repository
//!    identity; `thread.user-input.respond|dismiss` load the request's durable activity;
//! 3. decide, then stamp `metadata.origin` on every event;
//! 4. one transaction: for each event append → decode → projection pipeline (nested, deferred
//!    cleanup); then the `accepted` receipt with the last sequence;
//! 5. after commit: apply the events to the command model, run the attachment cleanups,
//!    publish each event, answer `{sequence}`;
//! 6. on failure (except the two receipt errors): re-read the events persisted since the
//!    dispatch started (another process may have written), project and publish them; a
//!    domain rejection then gets a `rejected` receipt.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures::{FutureExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use zc_contracts::{
    ApprovalRequestId, OrchestrationClientOrigin, OrchestrationCommand, OrchestrationEvent, OrchestrationReadModel, OrchestrationThreadActivity, ProjectId,
    RepositoryIdentity, ThreadId,
};
use zc_core::pubsub::PubSub;
use zc_db::repos::command_receipts::{self, CommandReceipt};
use zc_db::repos::event_store::{self, AggregateRange, AggregateReplayStats, EventPager};
use zc_db::{Db, DbError};
use zc_ports::{DispatchResult, EventStream, OrchestrationDispatch, ProjectionReads, TaggedError};

use crate::command::{aggregate_ref, command_id, command_type};
use crate::decider::{decide_orchestration_command, DeciderEnv, SystemEnv};
use crate::errors::{persistence_tagged, OrchestrationDispatchError};
use crate::event::{decode_persisted_event, OrchestrationEventExt};
use crate::pipeline::{DeferredCleanup, ProjectionPipeline};
use crate::projector::{create_empty_read_model, project_event};

/// The reads the engine needs from the projection tables (`ProjectionSnapshotQuery`):
/// `getCommandReadModel` at start, `getProjectShellById` for legacy link edits and
/// `getUserInputActivity` for question answers.
#[async_trait]
pub trait EngineReads: Send + Sync {
    /// `getCommandReadModel()`.
    async fn get_command_read_model(&self) -> Result<OrchestrationReadModel, TaggedError>;
    /// `getProjectShellById(projectId)?.repositoryIdentity`: `None` when there is no such
    /// project, `Some(None)` when it has no (or a null) identity.
    async fn get_project_repository_identity(&self, project_id: &ProjectId) -> Result<Option<Option<RepositoryIdentity>>, TaggedError>;
    /// `getUserInputActivity({threadId, requestId})`: the latest `user-input.requested` or
    /// `user-input.resolved` activity of the request.
    async fn get_user_input_activity(&self, thread_id: &ThreadId, request_id: &ApprovalRequestId) -> Result<Option<OrchestrationThreadActivity>, TaggedError>;
}

/// [`EngineReads`] over the [`ProjectionReads`] port (WP-09's snapshot queries).
pub struct ProjectionEngineReads(pub Arc<dyn ProjectionReads>);

#[async_trait]
impl EngineReads for ProjectionEngineReads {
    async fn get_command_read_model(&self) -> Result<OrchestrationReadModel, TaggedError> {
        self.0.get_command_read_model().await
    }
    async fn get_project_repository_identity(&self, project_id: &ProjectId) -> Result<Option<Option<RepositoryIdentity>>, TaggedError> {
        Ok(self
            .0
            .get_project_shell_by_id(project_id)
            .await?
            .map(|shell| shell.repository_identity.flatten()))
    }
    async fn get_user_input_activity(&self, thread_id: &ThreadId, request_id: &ApprovalRequestId) -> Result<Option<OrchestrationThreadActivity>, TaggedError> {
        self.0.get_user_input_activity(thread_id, request_id).await
    }
}

/// [`EngineReads`] straight from the event log, for an engine without projection tables
/// (tests, replays, tools): the command model is the whole log folded through the projector,
/// no project has a resolved repository identity, and a request's activity is the last
/// `user-input.requested|resolved` activity appended for it.
pub struct EventLogReads {
    db: Db,
}

impl EventLogReads {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Every stored event, decoded (`readAll`).
    pub async fn read_all_events(db: &Db) -> Result<Vec<OrchestrationEvent>, DbError> {
        let rows = db.call(event_store::read_all).await?;
        rows.iter()
            .map(|row| decode_persisted_event(row, "OrchestrationEventStore.readAll:rowToEvent"))
            .collect()
    }

    /// The command read model of a log, built from an empty model.
    pub fn fold(events: &[OrchestrationEvent], now_iso: &str) -> OrchestrationReadModel {
        let mut model = create_empty_read_model(now_iso);
        for event in events {
            project_event(&mut model, event);
        }
        model
    }
}

#[async_trait]
impl EngineReads for EventLogReads {
    async fn get_command_read_model(&self) -> Result<OrchestrationReadModel, TaggedError> {
        let events = Self::read_all_events(&self.db).await.map_err(|error| persistence_tagged(&error))?;
        Ok(Self::fold(&events, &zc_core::time::now_iso()))
    }

    async fn get_project_repository_identity(&self, _project_id: &ProjectId) -> Result<Option<Option<RepositoryIdentity>>, TaggedError> {
        Ok(None)
    }

    async fn get_user_input_activity(&self, thread_id: &ThreadId, request_id: &ApprovalRequestId) -> Result<Option<OrchestrationThreadActivity>, TaggedError> {
        let range = AggregateRange {
            aggregate_kind: "thread".into(),
            aggregate_id: thread_id.0.clone(),
            from_sequence_exclusive: 0,
            to_sequence_inclusive: i64::MAX,
        };
        let rows = self
            .db
            .call(move |conn| event_store::read_aggregate_range(conn, &range, Some(i64::MAX)))
            .await
            .map_err(|error| persistence_tagged(&error))?;
        let mut latest = None;
        for row in rows {
            if row.event_type == "thread.created" {
                latest = None;
            }
            if row.event_type != "thread.activity-appended" {
                continue;
            }
            let activity = &row.payload["activity"];
            let kind = activity["kind"].as_str().unwrap_or("");
            if (kind == "user-input.requested" || kind == "user-input.resolved") && activity["payload"]["requestId"].as_str() == Some(request_id.as_str()) {
                latest = serde_json::from_value::<OrchestrationThreadActivity>(activity.clone()).ok();
            }
        }
        Ok(latest)
    }
}

/// `ThreadBackgroundLivenessService.getThreadBackgroundLiveness(threadId) !== null`.
pub trait BackgroundLiveness: Send + Sync {
    fn has_live_background_work(&self, thread_id: &ThreadId) -> bool;
}

/// No thread ever has live background work.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoBackgroundLiveness;

impl BackgroundLiveness for NoBackgroundLiveness {
    fn has_live_background_work(&self, _thread_id: &ThreadId) -> bool {
        false
    }
}

/// What the engine is built from.
pub struct EngineConfig {
    pub db: Db,
    pub reads: Arc<dyn EngineReads>,
    pub pipeline: Arc<dyn ProjectionPipeline>,
    pub liveness: Arc<dyn BackgroundLiveness>,
    pub env: Arc<dyn DeciderEnv>,
}

impl EngineConfig {
    /// An engine over `db` alone: the no-op projection pipeline, the event log as the source
    /// of the command model, no background work, the system clock.
    pub fn standalone(db: Db) -> Self {
        Self {
            reads: Arc::new(EventLogReads::new(db.clone())),
            db,
            pipeline: Arc::new(crate::pipeline::NoopProjectionPipeline),
            liveness: Arc::new(NoBackgroundLiveness),
            env: Arc::new(SystemEnv),
        }
    }
}

type DispatchReply = oneshot::Sender<Result<DispatchResult, OrchestrationDispatchError>>;

enum Job {
    Dispatch {
        command: Box<OrchestrationCommand>,
        origin: Option<OrchestrationClientOrigin>,
        reply: DispatchReply,
    },
    ReadModel(oneshot::Sender<OrchestrationReadModel>),
}

struct Shared {
    db: Db,
    events: PubSub<OrchestrationEvent>,
    latest_sequence: AtomicI64,
}

/// `OrchestrationEngineService`. Cloning shares the engine; the worker stops when the last
/// clone is dropped and the queue is drained.
#[derive(Clone)]
pub struct OrchestrationEngine {
    queue: mpsc::UnboundedSender<Job>,
    shared: Arc<Shared>,
}

impl OrchestrationEngine {
    /// `makeOrchestrationEngine`: bootstraps the projection pipeline, loads the command read
    /// model, and starts the worker. Must run inside a Tokio runtime.
    pub async fn start(config: EngineConfig) -> Result<Self, OrchestrationDispatchError> {
        config.pipeline.bootstrap().await?;
        let model = config.reads.get_command_read_model().await.map_err(OrchestrationDispatchError::from_tagged)?;
        let shared = Arc::new(Shared {
            db: config.db.clone(),
            events: PubSub::new(),
            latest_sequence: AtomicI64::new(model.snapshot_sequence),
        });
        let (queue, jobs) = mpsc::unbounded_channel();
        let worker = Worker {
            shared: shared.clone(),
            reads: config.reads,
            pipeline: config.pipeline,
            liveness: config.liveness,
            env: config.env,
            model,
        };
        tracing::debug!(sequence = worker.model.snapshot_sequence, "orchestration engine started");
        tokio::spawn(worker.run(jobs));
        Ok(Self { queue, shared })
    }

    /// `dispatch(command, {origin})`.
    pub async fn dispatch(
        &self,
        command: OrchestrationCommand,
        origin: Option<OrchestrationClientOrigin>,
    ) -> Result<DispatchResult, OrchestrationDispatchError> {
        let (reply, response) = oneshot::channel();
        self.queue
            .send(Job::Dispatch {
                command: Box::new(command),
                origin,
                reply,
            })
            .map_err(|_| OrchestrationDispatchError::Persistence(DbError::Closed))?;
        response.await.map_err(|_| OrchestrationDispatchError::Persistence(DbError::Closed))?
    }

    /// `subscribeDomainEvents`: every event committed from now on, in order, unbounded.
    pub fn subscribe(&self) -> zc_core::pubsub::Subscription<OrchestrationEvent> {
        self.shared.events.subscribe()
    }

    /// `latestSequence`: the command read model's `snapshotSequence`.
    pub fn latest_sequence(&self) -> i64 {
        self.shared.latest_sequence.load(Ordering::SeqCst)
    }

    /// A copy of the command read model, after every command queued before this call.
    pub async fn command_read_model(&self) -> Result<OrchestrationReadModel, OrchestrationDispatchError> {
        let (reply, response) = oneshot::channel();
        self.queue
            .send(Job::ReadModel(reply))
            .map_err(|_| OrchestrationDispatchError::Persistence(DbError::Closed))?;
        response.await.map_err(|_| OrchestrationDispatchError::Persistence(DbError::Closed))
    }

    /// `readEvents(fromSequenceExclusive, limit = 1000)`.
    pub fn read_events(&self, from_sequence_exclusive: i64, limit: Option<i64>) -> EventStream<Result<OrchestrationEvent, DbError>> {
        decode_stream(
            EventPager::from_sequence(from_sequence_exclusive, limit).into_stream(self.shared.db.clone()),
            "OrchestrationEventStore.readFromSequence:rowToEvent",
        )
    }

    /// `readThreadEvents({threadId, fromSequenceExclusive, toSequenceInclusive, limit})`.
    pub fn read_thread_events(
        &self,
        thread_id: &ThreadId,
        from_sequence_exclusive: i64,
        to_sequence_inclusive: i64,
        limit: Option<i64>,
    ) -> EventStream<Result<OrchestrationEvent, DbError>> {
        let range = AggregateRange {
            aggregate_kind: "thread".into(),
            aggregate_id: thread_id.0.clone(),
            from_sequence_exclusive,
            to_sequence_inclusive,
        };
        decode_stream(
            EventPager::aggregate_range(range, limit).into_stream(self.shared.db.clone()),
            "OrchestrationEventStore.readAggregateRange:rowToEvent",
        )
    }

    /// `getThreadReplayStats({threadId, …range, maxEvents})`, with `hasCreateEvent`.
    pub async fn thread_replay_stats(
        &self,
        thread_id: &ThreadId,
        from_sequence_exclusive: i64,
        to_sequence_inclusive: i64,
        max_events: i64,
    ) -> Result<AggregateReplayStats, DbError> {
        let range = AggregateRange {
            aggregate_kind: "thread".into(),
            aggregate_id: thread_id.0.clone(),
            from_sequence_exclusive,
            to_sequence_inclusive,
        };
        self.shared
            .db
            .call(move |conn| event_store::get_aggregate_replay_stats(conn, &range, max_events))
            .await
    }
}

fn decode_stream(
    rows: impl futures::Stream<Item = Result<event_store::PersistedEvent, DbError>> + Send + 'static,
    operation: &'static str,
) -> EventStream<Result<OrchestrationEvent, DbError>> {
    rows.map(move |row| row.and_then(|row| decode_persisted_event(&row, operation))).boxed()
}

impl OrchestrationDispatchError {
    /// A tagged persistence error from a port read.
    pub fn from_tagged(error: TaggedError) -> Self {
        let operation = error
            .fields
            .get("operation")
            .and_then(Value::as_str)
            .unwrap_or("ProjectionSnapshotQuery")
            .to_owned();
        if error.tag == "PersistenceDecodeError" {
            let issue = error.fields.get("issue").and_then(Value::as_str).unwrap_or("").to_owned();
            return Self::Persistence(DbError::decode(operation, issue));
        }
        Self::Persistence(DbError::Sql {
            operation,
            detail: error.fields.get("detail").and_then(Value::as_str).map(str::to_owned),
            kind: zc_db::SqlErrorKind::Unknown,
            correlation: None,
            cause: None,
        })
    }
}

struct Worker {
    shared: Arc<Shared>,
    reads: Arc<dyn EngineReads>,
    pipeline: Arc<dyn ProjectionPipeline>,
    liveness: Arc<dyn BackgroundLiveness>,
    env: Arc<dyn DeciderEnv>,
    model: OrchestrationReadModel,
}

struct Committed {
    events: Vec<OrchestrationEvent>,
    cleanups: Vec<DeferredCleanup>,
    last_sequence: i64,
}

impl Worker {
    async fn run(mut self, mut jobs: mpsc::UnboundedReceiver<Job>) {
        while let Some(job) = jobs.recv().await {
            match job {
                Job::ReadModel(reply) => {
                    let _ = reply.send(self.model.clone());
                }
                Job::Dispatch { command, origin, reply } => {
                    let outcome = AssertUnwindSafe(self.process(&command, origin)).catch_unwind().await;
                    let result = match outcome {
                        Ok(result) => result,
                        Err(panic) => {
                            let message = panic
                                .downcast_ref::<&str>()
                                .map(|message| (*message).to_owned())
                                .or_else(|| panic.downcast_ref::<String>().cloned())
                                .unwrap_or_else(|| "panic".to_owned());
                            tracing::error!(command_type = command_type(&command), %message, "orchestration command panicked");
                            Err(OrchestrationDispatchError::Persistence(DbError::Panicked(message)))
                        }
                    };
                    let _ = reply.send(result);
                }
            }
        }
    }

    /// `processEnvelope`.
    async fn process(
        &mut self,
        command: &OrchestrationCommand,
        origin: Option<OrchestrationClientOrigin>,
    ) -> Result<DispatchResult, OrchestrationDispatchError> {
        let dispatch_start_sequence = self.model.snapshot_sequence;
        let result = self.try_process(command, origin).await;
        let error = match result {
            Ok(result) => return Ok(result),
            Err(error) => error,
        };
        if matches!(
            error,
            OrchestrationDispatchError::PreviouslyRejected { .. } | OrchestrationDispatchError::CommandIdConflict { .. }
        ) {
            return Err(error);
        }
        if let Err(reconcile_error) = self.reconcile(dispatch_start_sequence).await {
            tracing::warn!(
                command_id = command_id(command).as_str(),
                snapshot_sequence = self.model.snapshot_sequence,
                error = %reconcile_error,
                "failed to reconcile orchestration read model after dispatch failure"
            );
        }
        if let Some(rejection) = error.rejection() {
            let (aggregate_kind, aggregate_id) = aggregate_ref(command);
            let receipt = CommandReceipt {
                command_id: command_id(command).0.clone(),
                aggregate_kind: aggregate_kind.as_str().to_owned(),
                aggregate_id,
                accepted_at: self.env.now_iso(),
                result_sequence: self.model.snapshot_sequence,
                status: "rejected".into(),
                error: Some(rejection.to_string()),
            };
            if let Err(receipt_error) = self.shared.db.call(move |conn| command_receipts::upsert(conn, &receipt)).await {
                tracing::debug!(error = %receipt_error, "failed to record the rejected command receipt");
            }
        }
        Err(error)
    }

    /// `reconcileReadModelAfterDispatchFailure`.
    async fn reconcile(&mut self, from_sequence_exclusive: i64) -> Result<(), DbError> {
        let rows = self
            .shared
            .db
            .call(move |conn| event_store::read_from_sequence(conn, from_sequence_exclusive, None))
            .await?;
        if rows.is_empty() {
            return Ok(());
        }
        let events = rows
            .iter()
            .map(|row| decode_persisted_event(row, "OrchestrationEventStore.readFromSequence:rowToEvent"))
            .collect::<Result<Vec<_>, _>>()?;
        for event in &events {
            project_event(&mut self.model, event);
        }
        self.shared.latest_sequence.store(self.model.snapshot_sequence, Ordering::SeqCst);
        for event in events {
            self.shared.events.publish(event);
        }
        Ok(())
    }

    async fn has_event_after(&self, thread_id: &ThreadId, sequence_exclusive: i64, event_type: Option<&'static str>) -> Result<bool, DbError> {
        let thread_id = thread_id.0.clone();
        self.shared
            .db
            .call(move |conn| event_store::has_event_after(conn, "thread", &thread_id, event_type, sequence_exclusive))
            .await
    }

    async fn try_process(
        &mut self,
        command: &OrchestrationCommand,
        origin: Option<OrchestrationClientOrigin>,
    ) -> Result<DispatchResult, OrchestrationDispatchError> {
        use OrchestrationCommand as C;
        let kind = command_type(command);
        let id = command_id(command).0.clone();
        let (aggregate_kind, aggregate_id) = aggregate_ref(command);

        let lookup_id = id.clone();
        let receipt = self.shared.db.call(move |conn| command_receipts::get_by_command_id(conn, &lookup_id)).await?;
        if let Some(receipt) = receipt {
            // A receipt only proves this exact command was handled; replaying it for another
            // aggregate would report success for work that never happened.
            if receipt.aggregate_kind != aggregate_kind.as_str() || receipt.aggregate_id != aggregate_id {
                return Err(OrchestrationDispatchError::CommandIdConflict {
                    command_id: id,
                    receipt_aggregate_kind: receipt.aggregate_kind,
                    receipt_aggregate_id: receipt.aggregate_id,
                    command_aggregate_kind: aggregate_kind.as_str().to_owned(),
                    command_aggregate_id: aggregate_id,
                });
            }
            if receipt.status == "accepted" {
                return Ok(DispatchResult {
                    sequence: receipt.result_sequence,
                });
            }
            return Err(OrchestrationDispatchError::PreviouslyRejected {
                command_id: id,
                detail: receipt.error.unwrap_or_else(|| "Previously rejected.".into()),
            });
        }

        if let C::ThreadAutoSettle(auto) = command {
            if self.has_event_after(&auto.thread_id, auto.snapshot_sequence, None).await? {
                return Err(OrchestrationDispatchError::invariant(
                    kind,
                    format!("thread {} changed before automatic settlement", auto.thread_id),
                ));
            }
        }
        // The decider compares the lookup inputs; only recreation needs an event check.
        if let C::ThreadPullRequestSync(sync) | C::ThreadPullRequestSync_(sync) = command {
            if self.has_event_after(&sync.thread_id, sync.snapshot_sequence, Some("thread.created")).await? {
                return Err(OrchestrationDispatchError::invariant(
                    kind,
                    format!("thread {} was recreated before pull request discovery", sync.thread_id),
                ));
            }
        }
        if let C::ThreadAutoSettle(auto) = command {
            if self.liveness.has_live_background_work(&auto.thread_id) {
                return Err(OrchestrationDispatchError::invariant(
                    kind,
                    format!("thread {} has live background work", auto.thread_id),
                ));
            }
        }
        // New and moved projects carry no resolved identity in the event-derived model;
        // legacy PR edits need it to identify the link they replace.
        if let C::ClientOrchestrationCommandThreadMetaUpdate(meta) = command {
            if meta.linked_pull_request.is_some() {
                let project_id = self
                    .model
                    .threads
                    .iter()
                    .find(|thread| thread.id == meta.thread_id)
                    .map(|thread| thread.project_id.clone());
                if let Some(project_id) = project_id {
                    let identity = self
                        .reads
                        .get_project_repository_identity(&project_id)
                        .await
                        .map_err(OrchestrationDispatchError::from_tagged)?;
                    if let Some(identity) = identity {
                        for project in self.model.projects.iter_mut().filter(|project| project.id == project_id) {
                            project.repository_identity = identity.clone().map(Some);
                        }
                    }
                }
            }
        }
        // Command snapshots cap activities: read the request's durable state first.
        let user_input_activity = match command {
            C::ClientOrchestrationCommandThreadUserInputRespond(respond) => self
                .reads
                .get_user_input_activity(&respond.thread_id, &respond.request_id)
                .await
                .map_err(OrchestrationDispatchError::from_tagged)?,
            C::ClientOrchestrationCommandThreadUserInputDismiss(dismiss) => self
                .reads
                .get_user_input_activity(&dismiss.thread_id, &dismiss.request_id)
                .await
                .map_err(OrchestrationDispatchError::from_tagged)?,
            _ => None,
        };

        let mut planned = decide_orchestration_command(command, &self.model, user_input_activity.as_ref(), self.env.as_ref())?;
        // Attribution is an engine concern: stamp the dispatching client's origin.
        if let Some(origin) = origin {
            for event in &mut planned {
                event.base.metadata.origin = Some(origin.clone());
            }
        }

        let pipeline = self.pipeline.clone();
        let receipt_command_id = id.clone();
        let receipt_kind = kind.to_owned();
        let committed = self
            .shared
            .db
            .call(move |conn| {
                Ok(conn.immediate_transaction(|conn| -> Result<Committed, OrchestrationDispatchError> {
                    let mut events = Vec::with_capacity(planned.len());
                    let mut cleanups = Vec::new();
                    for next in &planned {
                        let row = event_store::append(conn, &next.to_new_event())?;
                        let saved = decode_persisted_event(&row, "OrchestrationEventStore.append:rowToEvent")?;
                        let cleanup = pipeline.project_event_deferred(conn, &saved)?;
                        if !cleanup.is_none() {
                            cleanups.push(cleanup);
                        }
                        events.push(saved);
                    }
                    let Some(last) = events.last() else {
                        return Err(OrchestrationDispatchError::invariant(receipt_kind, "Command produced no events."));
                    };
                    command_receipts::upsert(
                        conn,
                        &CommandReceipt {
                            command_id: receipt_command_id,
                            aggregate_kind: last.aggregate_kind().as_str().to_owned(),
                            aggregate_id: last.aggregate_id().to_owned(),
                            accepted_at: last.occurred_at().to_owned(),
                            result_sequence: last.sequence(),
                            status: "accepted".into(),
                            error: None,
                        },
                    )?;
                    let last_sequence = last.sequence();
                    Ok(Committed {
                        events,
                        cleanups,
                        last_sequence,
                    })
                }))
            })
            .await??;

        for event in &committed.events {
            project_event(&mut self.model, event);
        }
        self.shared.latest_sequence.store(self.model.snapshot_sequence, Ordering::SeqCst);
        for cleanup in committed.cleanups {
            if let Err(error) = tokio::task::spawn_blocking(move || cleanup.run()).await {
                tracing::warn!(%error, "attachment cleanup panicked");
            }
        }
        for event in committed.events {
            self.shared.events.publish(event);
        }
        Ok(DispatchResult {
            sequence: committed.last_sequence,
        })
    }
}

#[async_trait]
impl OrchestrationDispatch for OrchestrationEngine {
    async fn dispatch(&self, command: OrchestrationCommand, origin: Option<OrchestrationClientOrigin>) -> Result<DispatchResult, TaggedError> {
        OrchestrationEngine::dispatch(self, command, origin).await.map_err(TaggedError::from)
    }

    fn subscribe_domain_events(&self) -> EventStream<OrchestrationEvent> {
        self.subscribe().boxed()
    }

    async fn latest_sequence(&self) -> i64 {
        OrchestrationEngine::latest_sequence(self)
    }

    fn read_events(&self, from_sequence_exclusive: i64, limit: Option<u32>) -> EventStream<Result<OrchestrationEvent, TaggedError>> {
        OrchestrationEngine::read_events(self, from_sequence_exclusive, limit.map(i64::from))
            .map(|event| event.map_err(|error| persistence_tagged(&error)))
            .boxed()
    }

    fn read_thread_events(
        &self,
        range: zc_ports::orchestration::ThreadReplayRange,
        limit: Option<u32>,
    ) -> EventStream<Result<OrchestrationEvent, TaggedError>> {
        OrchestrationEngine::read_thread_events(
            self,
            &range.thread_id,
            range.from_sequence_exclusive,
            range.to_sequence_inclusive,
            limit.map(i64::from),
        )
        .map(|event| event.map_err(|error| persistence_tagged(&error)))
        .boxed()
    }

    async fn get_thread_replay_stats(
        &self,
        range: zc_ports::orchestration::ThreadReplayRange,
        max_events: u32,
    ) -> Result<zc_ports::orchestration::ThreadReplayStats, TaggedError> {
        let stats = self
            .thread_replay_stats(
                &range.thread_id,
                range.from_sequence_exclusive,
                range.to_sequence_inclusive,
                i64::from(max_events),
            )
            .await
            .map_err(|error| persistence_tagged(&error))?;
        Ok(zc_ports::orchestration::ThreadReplayStats {
            event_count: stats.event_count.max(0) as u64,
            payload_bytes: stats.payload_bytes.max(0) as u64,
            has_create_event: stats.has_create_event,
        })
    }
}
