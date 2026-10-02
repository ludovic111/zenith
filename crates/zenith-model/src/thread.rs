//! One thread in full, as `orchestration.subscribeThread` delivers it: a snapshot, then
//! events, applied with the server's own projector (`zc_orchestration::project_event`) so the
//! client and the server agree on every field.

use serde_json::Value;
use zc_contracts::{OrchestrationEvent, OrchestrationReadModel, OrchestrationSessionStatus, OrchestrationThread, OrchestrationThreadStreamItem, ThreadId};
use zc_orchestration::{project_event, OrchestrationEventExt};

#[derive(Clone, Debug)]
pub struct ThreadState {
    pub id: ThreadId,
    model: OrchestrationReadModel,
    /// A snapshot arrived.
    pub loaded: bool,
    /// The stream caught up.
    pub live: bool,
    /// Older turns exist beyond the loaded window.
    pub has_more: bool,
}

/// What an applied item changed, so views can react (scroll to the bottom on a new message).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub snapshot: bool,
    pub message: bool,
    pub activity: bool,
}

impl ThreadState {
    pub fn new(id: ThreadId) -> Self {
        Self {
            id,
            model: zc_orchestration::create_empty_read_model(String::new()),
            loaded: false,
            live: false,
            has_more: false,
        }
    }

    /// The thread, once a snapshot arrived (and while it is not deleted).
    pub fn thread(&self) -> Option<&OrchestrationThread> {
        self.model.threads.iter().find(|t| t.id == self.id && t.deleted_at.is_none())
    }

    pub fn sequence(&self) -> i64 {
        self.model.snapshot_sequence
    }

    /// Applies one stream item.
    pub fn apply(&mut self, item: Value) -> Result<Applied, String> {
        let item: OrchestrationThreadStreamItem = serde_json::from_value(item).map_err(|e| format!("thread item: {e}"))?;
        let mut applied = Applied::default();
        match item {
            OrchestrationThreadStreamItem::Synchronized(_) => self.live = true,
            OrchestrationThreadStreamItem::Snapshot(snapshot) => {
                let snapshot = snapshot.snapshot;
                self.model.snapshot_sequence = snapshot.snapshot_sequence;
                self.model.threads = vec![snapshot.thread];
                self.has_more = snapshot.page.as_ref().is_some_and(|p| p.has_more);
                self.loaded = true;
                applied.snapshot = true;
            }
            OrchestrationThreadStreamItem::Event(event) => {
                let event = event.event;
                if event.sequence() <= self.sequence() {
                    return Ok(applied);
                }
                applied.message = matches!(event, OrchestrationEvent::ThreadMessageSent(_));
                applied.activity = matches!(event, OrchestrationEvent::ThreadActivityAppended(_));
                project_event(&mut self.model, &event);
            }
        }
        Ok(applied)
    }

    /// The session runs a turn now.
    pub fn is_running(&self) -> bool {
        self.thread()
            .and_then(|t| t.session.as_ref())
            .is_some_and(|s| matches!(s.status, OrchestrationSessionStatus::Running | OrchestrationSessionStatus::Starting))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn snapshot_item(thread_extra: Value) -> Value {
        let mut thread = json!({
            "id": "t1", "projectId": "p1", "title": "Made-up thread",
            "modelSelection": {"instanceId": "claudeAgent", "model": "made-up-model"},
            "runtimeMode": "full-access", "interactionMode": "default",
            "branch": null, "worktreePath": null, "pullRequests": [], "latestTurn": null,
            "createdAt": "2026-10-01T10:00:00.000Z", "updatedAt": "2026-10-01T10:00:00.000Z",
            "archivedAt": null, "settledAt": null, "deletedAt": null,
            "messages": [], "proposedPlans": [], "activities": [], "checkpoints": [], "session": null
        });
        for (k, v) in thread_extra.as_object().unwrap() {
            thread[k] = v.clone();
        }
        json!({"kind": "snapshot", "snapshot": {"snapshotSequence": 10, "thread": thread}})
    }

    fn message_event(sequence: i64, text: &str, streaming: bool) -> Value {
        json!({"kind": "event", "event": {
            "sequence": sequence, "eventId": format!("e{sequence}"), "aggregateKind": "thread", "aggregateId": "t1",
            "occurredAt": "2026-10-01T10:00:01.000Z", "commandId": null, "causationEventId": null, "correlationId": null,
            "metadata": {}, "type": "thread.message-sent",
            "payload": {"threadId": "t1", "messageId": "m1", "role": "assistant", "text": text, "turnId": null,
                        "streaming": streaming, "createdAt": "2026-10-01T10:00:01.000Z", "updatedAt": "2026-10-01T10:00:01.000Z"}
        }})
    }

    #[test]
    fn streaming_messages_append_and_old_events_are_ignored() {
        let mut state = ThreadState::new(ThreadId::from("t1"));
        assert!(state.apply(snapshot_item(json!({}))).unwrap().snapshot);
        assert!(state.apply(message_event(11, "Hel", true)).unwrap().message);
        state.apply(message_event(12, "lo", true)).unwrap();
        // Replayed: ignored.
        state.apply(message_event(12, "lo", true)).unwrap();
        let thread = state.thread().unwrap();
        assert_eq!(thread.messages.len(), 1);
        assert_eq!(thread.messages[0].text, "Hello");
        assert_eq!(state.sequence(), 12);
        state.apply(json!({"kind": "synchronized"})).unwrap();
        assert!(state.live);
    }
}
