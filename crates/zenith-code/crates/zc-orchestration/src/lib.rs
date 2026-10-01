//! zc-orchestration (core): the Rust port of zenith code's event-sourced orchestration core
//! (WP-08 of `docs/zenith-code-rust-plan.md`, §5).
//!
//! | Module | Ported from |
//! |---|---|
//! | [`decider`] | `orchestration/decider.ts` (+ `ThreadSettlementPolicy.threadHasQueuedTurnStart`) |
//! | [`invariants`] | `orchestration/commandInvariants.ts` |
//! | [`errors`] | `orchestration/Errors.ts` |
//! | [`projector`] | `orchestration/projector.ts` (the in-memory command read model) |
//! | [`engine`] | `orchestration/Layers/OrchestrationEngine.ts` |
//! | [`pipeline`] | the `OrchestrationProjectionPipeline` boundary (`Services/ProjectionPipeline.ts`) |
//! | [`normalizer`] | `orchestration/Normalizer.ts` |
//! | [`attachments`] | `attachmentStore.ts`, `attachmentPaths.ts`, `imageMime.ts` (the parts used here) |
//! | [`command_ids`] | the server-side command id conventions (plan §5.2) |
//! | [`event`], [`command`], [`support`] | envelope accessors and the `packages/shared` helpers the above use |
//!
//! Wire types are the generated `zc_contracts` ones throughout: commands and events are
//! decoded once at the boundary, and the projector and decider work on typed values.

pub mod attachments;
pub mod command;
pub mod command_ids;
pub mod decider;
pub mod engine;
pub mod errors;
pub mod event;
pub mod invariants;
pub mod normalizer;
pub mod pipeline;
pub mod projector;
pub mod support;

pub use decider::{decide_orchestration_command, DeciderEnv, SystemEnv};
pub use engine::{BackgroundLiveness, EngineReads, EventLogReads, NoBackgroundLiveness, OrchestrationEngine};
pub use errors::{CommandRejection, OrchestrationDispatchError};
pub use event::{EventBase, EventPayload, OrchestrationEventExt, PlannedEvent};
pub use pipeline::{DeferredCleanup, NoopProjectionPipeline, ProjectionPipeline};
pub use projector::{create_empty_read_model, project_event};
