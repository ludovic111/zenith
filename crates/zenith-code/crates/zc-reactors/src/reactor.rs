//! `orchestration/Layers/OrchestrationReactor.ts`: starts every reactor, in this order:
//!
//! 1. provider runtime ingestion ([`crate::ProviderRuntimeIngestion`])
//! 2. provider command reactor ([`crate::ProviderCommandReactor`])
//! 3. checkpoint reactor (WP-11)
//! 4. thread deletion reactor ([`crate::ThreadDeletionReactor`])
//! 5. thread pull request reactor (WP-21)
//! 6. thread settlement reactor ([`crate::ThreadSettlementReactor`])
//! 7. pull request sync reactor (WP-21)
//! 8. agent awareness relay (T3 Connect, stub)
//! 9. storage cleanup (WP-11)
//!
//! Each start subscribes before it returns, so a later reactor never misses an event an
//! earlier one reacted to. Reactors owned by other work packages plug into their slot as
//! [`ExternalReactor`]s.

use std::sync::Arc;

use async_trait::async_trait;

use crate::{ProviderCommandReactor, ProviderRuntimeIngestion, ThreadDeletionReactor, ThreadSettlementReactor};

/// The start order slots of `OrchestrationReactor.start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReactorSlot {
    ProviderRuntimeIngestion,
    ProviderCommandReactor,
    CheckpointReactor,
    ThreadDeletionReactor,
    ThreadPullRequestReactor,
    ThreadSettlementReactor,
    PullRequestSyncReactor,
    AgentAwarenessRelay,
    StorageCleanup,
}

impl ReactorSlot {
    /// Every slot, in start order.
    pub const ORDER: [ReactorSlot; 9] = [
        ReactorSlot::ProviderRuntimeIngestion,
        ReactorSlot::ProviderCommandReactor,
        ReactorSlot::CheckpointReactor,
        ReactorSlot::ThreadDeletionReactor,
        ReactorSlot::ThreadPullRequestReactor,
        ReactorSlot::ThreadSettlementReactor,
        ReactorSlot::PullRequestSyncReactor,
        ReactorSlot::AgentAwarenessRelay,
        ReactorSlot::StorageCleanup,
    ];
}

/// A reactor another work package owns (checkpoints, PR sync, storage cleanup, …).
#[async_trait]
pub trait ExternalReactor: Send + Sync {
    /// Subscribes and starts its background work; returns once subscribed.
    async fn start(&self);
}

/// The reactors of one server, started together.
#[derive(Default)]
pub struct OrchestrationReactor {
    pub ingestion: Option<Arc<ProviderRuntimeIngestion>>,
    pub command_reactor: Option<Arc<ProviderCommandReactor>>,
    pub deletion: Option<Arc<ThreadDeletionReactor>>,
    pub settlement: Option<Arc<ThreadSettlementReactor>>,
    /// Reactors of other work packages, by slot.
    pub external: Vec<(ReactorSlot, Arc<dyn ExternalReactor>)>,
}

impl OrchestrationReactor {
    /// `start()`: every present reactor, in [`ReactorSlot::ORDER`]. Returns the slots started.
    pub async fn start(&self) -> Vec<ReactorSlot> {
        let mut started = Vec::new();
        for slot in ReactorSlot::ORDER {
            let mut ran = false;
            match slot {
                ReactorSlot::ProviderRuntimeIngestion => {
                    if let Some(ingestion) = &self.ingestion {
                        ingestion.start();
                        ran = true;
                    }
                }
                ReactorSlot::ProviderCommandReactor => {
                    if let Some(reactor) = &self.command_reactor {
                        reactor.start().await;
                        ran = true;
                    }
                }
                ReactorSlot::ThreadDeletionReactor => {
                    if let Some(reactor) = &self.deletion {
                        reactor.start().await;
                        ran = true;
                    }
                }
                ReactorSlot::ThreadSettlementReactor => {
                    if let Some(reactor) = &self.settlement {
                        reactor.start().await;
                        ran = true;
                    }
                }
                _ => {}
            }
            for (external_slot, reactor) in &self.external {
                if *external_slot == slot {
                    reactor.start().await;
                    ran = true;
                }
            }
            if ran {
                started.push(slot);
            }
        }
        started
    }

    /// Stops every owned reactor.
    pub fn stop(&self) {
        if let Some(ingestion) = &self.ingestion {
            ingestion.stop();
        }
        if let Some(reactor) = &self.command_reactor {
            reactor.stop();
        }
        if let Some(reactor) = &self.deletion {
            reactor.stop();
        }
        if let Some(reactor) = &self.settlement {
            reactor.stop();
        }
    }
}
