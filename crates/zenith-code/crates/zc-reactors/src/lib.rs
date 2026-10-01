//! zc-reactors: zenith code's orchestration reactors (WP-10 of
//! `docs/zenith-code-rust-plan.md`, §5.4): the processes that turn provider runtime events into
//! orchestration commands and orchestration events into provider calls.
//!
//! | Module | Ported from |
//! |---|---|
//! | [`ingestion`] | `orchestration/Layers/ProviderRuntimeIngestion.ts` |
//! | [`command_reactor`] | `orchestration/Layers/ProviderCommandReactor.ts` |
//! | [`deletion`] | `orchestration/Layers/ThreadDeletionReactor.ts` |
//! | [`settlement`] | `orchestration/ThreadSettlementReactor.ts`, `ThreadSettlementPolicy.ts` |
//! | [`reactor`] | `orchestration/Layers/OrchestrationReactor.ts` (start order) |
//! | [`registries`] | `ThreadBackgroundLiveness.ts`, `ThreadPlanProgress.ts` |
//! | [`activity_payload`] | `ActivityPayloadProjection.ts` `projectActivityPayload` |
//! | [`titles`], [`composer`], [`settings`] | `threadTitles.ts`, `ThreadTitleContext.ts`, shared `git.ts`, `composerContextReferences.ts`, `assistantCitations.ts`, `projectSettings.ts` |
//! | [`reads`], [`event_log_reads`] | the `ProjectionSnapshotQuery` and projection-repository reads the reactors use |
//! | [`runtime`], [`common`] | `DrainableWorker`, Effect `Cache`, `Clock` |
//!
//! The reactors depend only on `zc_ports` traits plus [`reads::ReactorReads`]; the server
//! wiring hands them the concrete services. Runtime events and commands cross as wire JSON (the
//! ports carry `ProviderRuntimeEvent` as JSON, and commands are decoded into
//! `zc_contracts::OrchestrationCommand` at dispatch), which keeps the activity payloads
//! byte-for-byte what the TS layer builds.

pub mod activity_payload;
pub mod command_reactor;
pub mod common;
pub mod composer;
pub mod deletion;
pub mod event_log_reads;
pub mod ingestion;
pub mod js;
pub mod reactor;
pub mod reads;
pub mod registries;
pub mod runtime;
pub mod settings;
pub mod settlement;
pub mod text_generation;
pub mod titles;
pub mod workspace_lease;

pub use command_reactor::{CommandReactorDeps, ProviderCommandReactor};
pub use common::{FixedRepositoryProbe, GitWorkflowRepositoryProbe, RepositoryProbe};
pub use deletion::ThreadDeletionReactor;
pub use event_log_reads::EventLogReactorReads;
pub use ingestion::{IngestionDeps, ProviderRuntimeIngestion};
pub use reactor::{OrchestrationReactor, ReactorSlot};
pub use reads::{ProjectionReactorReads, ReactorReads};
pub use registries::{ThreadBackgroundLivenessRegistry, ThreadPlanProgressRegistry};
pub use runtime::{ManualClock, ReactorClock, SystemClock};
pub use settlement::{SettlementDeps, ThreadSettlementReactor};
pub use text_generation::NoTextGeneration;
