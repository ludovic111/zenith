//! `RuntimeReceiptBus` (`orchestration/Services/RuntimeReceiptBus.ts`, `Layers/RuntimeReceiptBus.ts`):
//! checkpoint-reactor milestones that tests and harnesses can wait on. The live bus drops them;
//! the test bus broadcasts them.

use serde::Serialize;
use zc_contracts::{CheckpointRef, OrchestrationCheckpointStatus, ThreadId, TurnId};
use zc_core::pubsub::{PubSub, Subscription};

/// `OrchestrationRuntimeReceipt`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum OrchestrationRuntimeReceipt {
    #[serde(rename = "checkpoint.baseline.captured")]
    CheckpointBaselineCaptured {
        thread_id: ThreadId,
        checkpoint_turn_count: i64,
        checkpoint_ref: CheckpointRef,
        created_at: String,
    },
    #[serde(rename = "checkpoint.diff.finalized")]
    CheckpointDiffFinalized {
        thread_id: ThreadId,
        turn_id: TurnId,
        checkpoint_turn_count: i64,
        checkpoint_ref: CheckpointRef,
        status: OrchestrationCheckpointStatus,
        created_at: String,
    },
    #[serde(rename = "turn.processing.quiesced")]
    TurnProcessingQuiesced {
        thread_id: ThreadId,
        turn_id: TurnId,
        checkpoint_turn_count: i64,
        created_at: String,
    },
}

/// `RuntimeReceiptBus`.
#[derive(Clone, Default)]
pub struct RuntimeReceiptBus {
    events: Option<PubSub<OrchestrationRuntimeReceipt>>,
}

impl RuntimeReceiptBus {
    /// `RuntimeReceiptBusLive`: receipts are neither kept nor broadcast.
    pub fn live() -> Self {
        Self { events: None }
    }

    /// `RuntimeReceiptBusTest`: an unbounded broadcast.
    pub fn for_test() -> Self {
        Self { events: Some(PubSub::new()) }
    }

    /// `publish(receipt)`.
    pub fn publish(&self, receipt: OrchestrationRuntimeReceipt) {
        if let Some(events) = &self.events {
            events.publish(receipt);
        }
    }

    /// `streamEventsForTest`: receipts published from now on (none on the live bus).
    pub fn subscribe_for_test(&self) -> Option<Subscription<OrchestrationRuntimeReceipt>> {
        self.events.as_ref().map(PubSub::subscribe)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn the_test_bus_broadcasts_and_the_live_bus_drops() {
        let bus = RuntimeReceiptBus::for_test();
        let mut receipts = bus.subscribe_for_test().unwrap();
        bus.publish(OrchestrationRuntimeReceipt::TurnProcessingQuiesced {
            thread_id: ThreadId::new("t"),
            turn_id: TurnId::new("u"),
            checkpoint_turn_count: 1,
            created_at: "2026-01-01T00:00:00.000Z".into(),
        });
        let receipt = receipts.recv().await.unwrap();
        assert_eq!(
            serde_json::to_value(receipt).unwrap(),
            json!({"type": "turn.processing.quiesced", "threadId": "t", "turnId": "u", "checkpointTurnCount": 1, "createdAt": "2026-01-01T00:00:00.000Z"})
        );
        assert!(RuntimeReceiptBus::live().subscribe_for_test().is_none());
    }
}
