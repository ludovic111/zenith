//! Working with [`OrchestrationEvent`]s: the envelope accessors the generated enum lacks, the
//! "planned" event the decider produces (`Omit<OrchestrationEvent, "sequence">`), and the
//! conversions to and from the event store rows of zc-db.

use serde_json::Value;
use zc_contracts::*;
use zc_db::repos::event_store::{NewEvent, PersistedEvent};
use zc_db::DbError;

/// Runs `$body` with `$e` bound to the variant struct of any [`OrchestrationEvent`].
macro_rules! with_event {
    ($event:expr, $e:ident => $body:expr) => {
        match $event {
            OrchestrationEvent::ProjectCreated($e) => $body,
            OrchestrationEvent::ProjectMetaUpdated($e) => $body,
            OrchestrationEvent::ProjectDeleted($e) => $body,
            OrchestrationEvent::ThreadCreated($e) => $body,
            OrchestrationEvent::ThreadDeleted($e) => $body,
            OrchestrationEvent::ThreadArchived($e) => $body,
            OrchestrationEvent::ThreadUnarchived($e) => $body,
            OrchestrationEvent::ThreadSettled($e) => $body,
            OrchestrationEvent::ThreadUnsettled($e) => $body,
            OrchestrationEvent::ThreadSnoozed($e) => $body,
            OrchestrationEvent::ThreadUnsnoozed($e) => $body,
            OrchestrationEvent::ThreadPinned($e) => $body,
            OrchestrationEvent::ThreadUnpinned($e) => $body,
            OrchestrationEvent::ThreadPinReordered($e) => $body,
            OrchestrationEvent::ThreadAutoSettleSet($e) => $body,
            OrchestrationEvent::ThreadMetaUpdated($e) => $body,
            OrchestrationEvent::ThreadPullRequestLinked($e) => $body,
            OrchestrationEvent::ThreadPullRequestUnlinked($e) => $body,
            OrchestrationEvent::ThreadPullRequestSynced($e) => $body,
            OrchestrationEvent::ThreadRuntimeModeSet($e) => $body,
            OrchestrationEvent::ThreadInteractionModeSet($e) => $body,
            OrchestrationEvent::ThreadMessageSent($e) => $body,
            OrchestrationEvent::ThreadTurnStartRequested($e) => $body,
            OrchestrationEvent::ThreadTurnInterruptRequested($e) => $body,
            OrchestrationEvent::ThreadApprovalResponseRequested($e) => $body,
            OrchestrationEvent::ThreadUserInputResponseRequested($e) => $body,
            OrchestrationEvent::ThreadCheckpointRevertRequested($e) => $body,
            OrchestrationEvent::ThreadReverted($e) => $body,
            OrchestrationEvent::ThreadSessionStopRequested($e) => $body,
            OrchestrationEvent::ThreadSessionSet($e) => $body,
            OrchestrationEvent::ThreadProposedPlanUpserted($e) => $body,
            OrchestrationEvent::ThreadTurnDiffCompleted($e) => $body,
            OrchestrationEvent::ThreadActivityAppended($e) => $body,
        }
    };
}

/// The payload of an event, one variant per `OrchestrationEventType`, with the generated
/// payload types. Building an event means picking one of these and an [`EventBase`].
macro_rules! event_payloads {
    ($(($variant:ident, $event_struct:ident, $lit:ident, $payload:ident, $tag:literal)),* $(,)?) => {
        #[derive(Debug, Clone, PartialEq)]
        pub enum EventPayload {
            $($variant($payload),)*
        }

        impl EventPayload {
            /// The event `type` (`OrchestrationEventType`).
            pub fn event_type(&self) -> &'static str {
                match self {
                    $(Self::$variant(_) => $tag,)*
                }
            }

            /// The encoded payload.
            pub fn to_value(&self) -> Value {
                let result = match self {
                    $(Self::$variant(payload) => serde_json::to_value(payload),)*
                };
                result.expect("orchestration payloads always serialize")
            }
        }

        $(
            impl From<$payload> for EventPayload {
                fn from(payload: $payload) -> Self {
                    Self::$variant(payload)
                }
            }
        )*

        impl PlannedEvent {
            /// The persisted event, once the store has given it a sequence.
            pub fn into_event(self, sequence: i64) -> OrchestrationEvent {
                let EventBase {
                    event_id,
                    aggregate_kind,
                    aggregate_id,
                    occurred_at,
                    command_id,
                    causation_event_id,
                    correlation_id,
                    metadata,
                } = self.base;
                match self.payload {
                    $(EventPayload::$variant(payload) => OrchestrationEvent::$variant($event_struct {
                        sequence,
                        event_id,
                        aggregate_kind,
                        aggregate_id,
                        occurred_at,
                        command_id,
                        causation_event_id,
                        correlation_id,
                        metadata,
                        r#type: $lit,
                        payload,
                    }),)*
                }
            }
        }

        /// Splits a persisted event into its sequence and its planned form.
        pub fn split_event(event: OrchestrationEvent) -> (i64, PlannedEvent) {
            match event {
                $(OrchestrationEvent::$variant(e) => (
                    e.sequence,
                    PlannedEvent {
                        base: EventBase {
                            event_id: e.event_id,
                            aggregate_kind: e.aggregate_kind,
                            aggregate_id: e.aggregate_id,
                            occurred_at: e.occurred_at,
                            command_id: e.command_id,
                            causation_event_id: e.causation_event_id,
                            correlation_id: e.correlation_id,
                            metadata: e.metadata,
                        },
                        payload: EventPayload::$variant(e.payload),
                    },
                ),)*
            }
        }

        /// The `type` of a persisted event.
        pub fn event_type(event: &OrchestrationEvent) -> &'static str {
            match event {
                $(OrchestrationEvent::$variant(_) => $tag,)*
            }
        }

        /// The encoded payload of a persisted event.
        pub fn event_payload_value(event: &OrchestrationEvent) -> Value {
            let result = match event {
                $(OrchestrationEvent::$variant(e) => serde_json::to_value(&e.payload),)*
            };
            result.expect("orchestration payloads always serialize")
        }

        /// Every `OrchestrationEventType`, in declaration order.
        pub const EVENT_TYPES: &[&str] = &[$($tag),*];
    };
}

event_payloads! {
    (ProjectCreated, OrchestrationEventProjectCreated, LitProjectCreated, ProjectCreatedPayload, "project.created"),
    (ProjectMetaUpdated, OrchestrationEventProjectMetaUpdated, LitProjectMetaUpdated, ProjectMetaUpdatedPayload, "project.meta-updated"),
    (ProjectDeleted, OrchestrationEventProjectDeleted, LitProjectDeleted, ProjectDeletedPayload, "project.deleted"),
    (ThreadCreated, OrchestrationEventThreadCreated, LitThreadCreated, ThreadCreatedPayload, "thread.created"),
    (ThreadDeleted, OrchestrationEventThreadDeleted, LitThreadDeleted, ThreadDeletedPayload, "thread.deleted"),
    (ThreadArchived, OrchestrationEventThreadArchived, LitThreadArchived, ThreadArchivedPayload, "thread.archived"),
    (ThreadUnarchived, OrchestrationEventThreadUnarchived, LitThreadUnarchived, ThreadUnarchivedPayload, "thread.unarchived"),
    (ThreadSettled, OrchestrationEventThreadSettled, LitThreadSettled, ThreadSettledPayload, "thread.settled"),
    (ThreadUnsettled, OrchestrationEventThreadUnsettled, LitThreadUnsettled, ThreadUnsettledPayload, "thread.unsettled"),
    (ThreadSnoozed, OrchestrationEventThreadSnoozed, LitThreadSnoozed, ThreadSnoozedPayload, "thread.snoozed"),
    (ThreadUnsnoozed, OrchestrationEventThreadUnsnoozed, LitThreadUnsnoozed, ThreadUnsnoozedPayload, "thread.unsnoozed"),
    (ThreadPinned, OrchestrationEventThreadPinned, LitThreadPinned, ThreadPinnedPayload, "thread.pinned"),
    (ThreadUnpinned, OrchestrationEventThreadUnpinned, LitThreadUnpinned, ThreadUnpinnedPayload, "thread.unpinned"),
    (ThreadPinReordered, OrchestrationEventThreadPinReordered, LitThreadPinReordered, ThreadPinReorderedPayload, "thread.pin-reordered"),
    (ThreadAutoSettleSet, OrchestrationEventThreadAutoSettleSet, LitThreadAutoSettleSet2, ThreadAutoSettleSetPayload, "thread.auto-settle-set"),
    (ThreadMetaUpdated, OrchestrationEventThreadMetaUpdated, LitThreadMetaUpdated, ThreadMetaUpdatedPayload, "thread.meta-updated"),
    (ThreadPullRequestLinked, OrchestrationEventThreadPullRequestLinked, LitThreadPullRequestLinked, ThreadPullRequestLinkedPayload, "thread.pull-request-linked"),
    (ThreadPullRequestUnlinked, OrchestrationEventThreadPullRequestUnlinked, LitThreadPullRequestUnlinked, ThreadPullRequestUnlinkedPayload, "thread.pull-request-unlinked"),
    (ThreadPullRequestSynced, OrchestrationEventThreadPullRequestSynced, LitThreadPullRequestSynced, ThreadPullRequestSyncedPayload, "thread.pull-request-synced"),
    (ThreadRuntimeModeSet, OrchestrationEventThreadRuntimeModeSet, LitThreadRuntimeModeSet2, ThreadRuntimeModeSetPayload, "thread.runtime-mode-set"),
    (ThreadInteractionModeSet, OrchestrationEventThreadInteractionModeSet, LitThreadInteractionModeSet2, ThreadInteractionModeSetPayload, "thread.interaction-mode-set"),
    (ThreadMessageSent, OrchestrationEventThreadMessageSent, LitThreadMessageSent, ThreadMessageSentPayload, "thread.message-sent"),
    (ThreadTurnStartRequested, OrchestrationEventThreadTurnStartRequested, LitThreadTurnStartRequested, ThreadTurnStartRequestedPayload, "thread.turn-start-requested"),
    (ThreadTurnInterruptRequested, OrchestrationEventThreadTurnInterruptRequested, LitThreadTurnInterruptRequested, ThreadTurnInterruptRequestedPayload, "thread.turn-interrupt-requested"),
    (ThreadApprovalResponseRequested, OrchestrationEventThreadApprovalResponseRequested, LitThreadApprovalResponseRequested, ThreadApprovalResponseRequestedPayload, "thread.approval-response-requested"),
    (ThreadUserInputResponseRequested, OrchestrationEventThreadUserInputResponseRequested, LitThreadUserInputResponseRequested, OrchestrationEventThreadUserInputResponseRequestedPayload, "thread.user-input-response-requested"),
    (ThreadCheckpointRevertRequested, OrchestrationEventThreadCheckpointRevertRequested, LitThreadCheckpointRevertRequested, ThreadCheckpointRevertRequestedPayload, "thread.checkpoint-revert-requested"),
    (ThreadReverted, OrchestrationEventThreadReverted, LitThreadReverted, ThreadRevertedPayload, "thread.reverted"),
    (ThreadSessionStopRequested, OrchestrationEventThreadSessionStopRequested, LitThreadSessionStopRequested, ThreadSessionStopRequestedPayload, "thread.session-stop-requested"),
    (ThreadSessionSet, OrchestrationEventThreadSessionSet, LitThreadSessionSet2, ThreadSessionSetPayload, "thread.session-set"),
    (ThreadProposedPlanUpserted, OrchestrationEventThreadProposedPlanUpserted, LitThreadProposedPlanUpserted, ThreadProposedPlanUpsertedPayload, "thread.proposed-plan-upserted"),
    (ThreadTurnDiffCompleted, OrchestrationEventThreadTurnDiffCompleted, LitThreadTurnDiffCompleted, ThreadTurnDiffCompletedPayload, "thread.turn-diff-completed"),
    (ThreadActivityAppended, OrchestrationEventThreadActivityAppended, LitThreadActivityAppended, ThreadActivityAppendedPayload, "thread.activity-appended"),
}

/// The envelope of an event without its sequence and payload (`withEventBase` in
/// `decider.ts`).
#[derive(Debug, Clone, PartialEq)]
pub struct EventBase {
    pub event_id: EventId,
    pub aggregate_kind: OrchestrationAggregateKind,
    pub aggregate_id: ProjectIdOrThreadId,
    pub occurred_at: String,
    pub command_id: Option<CommandId>,
    pub causation_event_id: Option<EventId>,
    pub correlation_id: Option<CommandId>,
    pub metadata: OrchestrationEventMetadata,
}

/// `Omit<OrchestrationEvent, "sequence">`: what the decider plans and the store appends.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedEvent {
    pub base: EventBase,
    pub payload: EventPayload,
}

impl PlannedEvent {
    pub fn event_type(&self) -> &'static str {
        self.payload.event_type()
    }

    /// The row to append (`OrchestrationEventStore.append`'s input).
    pub fn to_new_event(&self) -> NewEvent {
        NewEvent {
            event_id: self.base.event_id.0.clone(),
            aggregate_kind: self.base.aggregate_kind.as_str().to_owned(),
            aggregate_id: aggregate_id_str(&self.base.aggregate_id).to_owned(),
            occurred_at: self.base.occurred_at.clone(),
            command_id: self.base.command_id.as_ref().map(|id| id.0.clone()),
            causation_event_id: self.base.causation_event_id.as_ref().map(|id| id.0.clone()),
            correlation_id: self.base.correlation_id.as_ref().map(|id| id.0.clone()),
            metadata: serde_json::to_value(&self.base.metadata).expect("event metadata always serializes"),
            event_type: self.event_type().to_owned(),
            payload: self.payload.to_value(),
        }
    }
}

/// Metadata with no keys (`{}`).
pub fn empty_metadata() -> OrchestrationEventMetadata {
    OrchestrationEventMetadata {
        provider_turn_id: None,
        provider_item_id: None,
        adapter_key: None,
        request_id: None,
        ingested_at: None,
        history_import: None,
        deferred_turn: None,
        origin: None,
    }
}

/// An aggregate id. The two members of `ProjectIdOrThreadId` are both plain strings on the
/// wire and decode as the first one, so every id is built that way: an event built here then
/// compares equal to the same event read back from the store.
pub fn aggregate_id(id: &str) -> ProjectIdOrThreadId {
    ProjectIdOrThreadId::ProjectId(ProjectId::new(id))
}

/// The plain string of an aggregate id.
pub fn aggregate_id_str(id: &ProjectIdOrThreadId) -> &str {
    match id {
        ProjectIdOrThreadId::ProjectId(id) => id.as_str(),
        ProjectIdOrThreadId::ThreadId(id) => id.as_str(),
    }
}

/// Envelope accessors shared by every [`OrchestrationEvent`] variant.
pub trait OrchestrationEventExt {
    fn sequence(&self) -> i64;
    fn set_sequence(&mut self, sequence: i64);
    fn event_id(&self) -> &EventId;
    fn aggregate_kind(&self) -> OrchestrationAggregateKind;
    fn aggregate_id(&self) -> &str;
    fn occurred_at(&self) -> &str;
    fn command_id(&self) -> Option<&CommandId>;
    fn causation_event_id(&self) -> Option<&EventId>;
    fn correlation_id(&self) -> Option<&CommandId>;
    fn metadata(&self) -> &OrchestrationEventMetadata;
    fn metadata_mut(&mut self) -> &mut OrchestrationEventMetadata;
    fn event_type(&self) -> &'static str;
    /// The thread the event belongs to (`payload.threadId`), for thread events.
    fn thread_id(&self) -> Option<&ThreadId>;
}

impl OrchestrationEventExt for OrchestrationEvent {
    fn sequence(&self) -> i64 {
        with_event!(self, e => e.sequence)
    }
    fn set_sequence(&mut self, sequence: i64) {
        with_event!(self, e => e.sequence = sequence)
    }
    fn event_id(&self) -> &EventId {
        with_event!(self, e => &e.event_id)
    }
    fn aggregate_kind(&self) -> OrchestrationAggregateKind {
        with_event!(self, e => e.aggregate_kind)
    }
    fn aggregate_id(&self) -> &str {
        with_event!(self, e => aggregate_id_str(&e.aggregate_id))
    }
    fn occurred_at(&self) -> &str {
        with_event!(self, e => &e.occurred_at)
    }
    fn command_id(&self) -> Option<&CommandId> {
        with_event!(self, e => e.command_id.as_ref())
    }
    fn causation_event_id(&self) -> Option<&EventId> {
        with_event!(self, e => e.causation_event_id.as_ref())
    }
    fn correlation_id(&self) -> Option<&CommandId> {
        with_event!(self, e => e.correlation_id.as_ref())
    }
    fn metadata(&self) -> &OrchestrationEventMetadata {
        with_event!(self, e => &e.metadata)
    }
    fn metadata_mut(&mut self) -> &mut OrchestrationEventMetadata {
        with_event!(self, e => &mut e.metadata)
    }
    fn event_type(&self) -> &'static str {
        event_type(self)
    }
    fn thread_id(&self) -> Option<&ThreadId> {
        match self {
            OrchestrationEvent::ProjectCreated(_) | OrchestrationEvent::ProjectMetaUpdated(_) | OrchestrationEvent::ProjectDeleted(_) => None,
            OrchestrationEvent::ThreadCreated(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadDeleted(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadArchived(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadUnarchived(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadSettled(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadUnsettled(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadSnoozed(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadUnsnoozed(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadPinned(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadUnpinned(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadPinReordered(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadAutoSettleSet(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadMetaUpdated(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadPullRequestLinked(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadPullRequestUnlinked(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadPullRequestSynced(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadRuntimeModeSet(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadInteractionModeSet(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadMessageSent(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadTurnStartRequested(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadTurnInterruptRequested(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadApprovalResponseRequested(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadUserInputResponseRequested(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadCheckpointRevertRequested(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadReverted(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadSessionStopRequested(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadSessionSet(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadProposedPlanUpserted(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadTurnDiffCompleted(e) => Some(&e.payload.thread_id),
            OrchestrationEvent::ThreadActivityAppended(e) => Some(&e.payload.thread_id),
        }
    }
}

/// Decodes a stored row into the wire event (`decodeEvent(row)` in the TS event store).
/// `operation` names the call site, as in `OrchestrationEventStore.append:rowToEvent`.
pub fn decode_persisted_event(row: &PersistedEvent, operation: &str) -> Result<OrchestrationEvent, DbError> {
    let value = serde_json::to_value(row).map_err(|error| DbError::decode(operation, error.to_string()))?;
    serde_json::from_value(value).map_err(|error| DbError::decode(operation, error.to_string()))
}

/// Decodes rows read from the store (`readFromSequence:rowToEvent`).
pub fn decode_persisted_events(rows: &[PersistedEvent], operation: &str) -> Result<Vec<OrchestrationEvent>, DbError> {
    rows.iter().map(|row| decode_persisted_event(row, operation)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn planned_events_round_trip_through_the_wire_shape() {
        let planned = PlannedEvent {
            base: EventBase {
                event_id: EventId::new("event-1"),
                aggregate_kind: OrchestrationAggregateKind::Thread,
                aggregate_id: aggregate_id("thread-1"),
                occurred_at: "2026-01-01T00:00:00.000Z".into(),
                command_id: Some(CommandId::new("cmd-1")),
                causation_event_id: None,
                correlation_id: Some(CommandId::new("cmd-1")),
                metadata: empty_metadata(),
            },
            payload: EventPayload::ThreadDeleted(ThreadDeletedPayload {
                thread_id: ThreadId::new("thread-1"),
                deleted_at: "2026-01-01T00:00:00.000Z".into(),
            }),
        };
        let row = planned.to_new_event();
        assert_eq!(row.event_type, "thread.deleted");
        assert_eq!(row.metadata, json!({}));
        let event = planned.clone().into_event(7);
        assert_eq!(event.sequence(), 7);
        assert_eq!(event.aggregate_id(), "thread-1");
        assert_eq!(event.thread_id().map(ThreadId::as_str), Some("thread-1"));
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["type"], "thread.deleted");
        assert_eq!(wire["payload"]["deletedAt"], "2026-01-01T00:00:00.000Z");
        let decoded: OrchestrationEvent = serde_json::from_value(wire).unwrap();
        assert_eq!(decoded, event);
        let (sequence, back) = split_event(decoded);
        assert_eq!(sequence, 7);
        assert_eq!(back, planned);
        assert_eq!(EVENT_TYPES.len(), 33);
    }
}
