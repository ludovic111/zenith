//! zc-projections: the orchestration read side of zenith code (WP-09 of
//! `docs/zenith-code-rust-plan.md`).
//!
//! | Module | Ported from |
//! |---|---|
//! | [`pipeline`] | `orchestration/Layers/ProjectionPipeline.ts`: the nine SQL projectors, cursors, bootstrap, attachment-cleanup cursor |
//! | [`query`] | `orchestration/Layers/ProjectionSnapshotQuery.ts`: every snapshot query, SQL verbatim |
//! | [`activity_payload`] | `orchestration/ActivityPayloadProjection.ts` (+ shared `projectQuestionToolInput`) |
//! | [`cursor`] | `orchestration/threadDetailCursor.ts` |
//! | [`budget`] | `orchestration/LiveStreamBudget.ts` |
//! | [`coalescer`] | `orchestration/ThreadLiveEventCoalescer.ts` |
//! | [`subscriptions`] | `ws.ts` `orchestration.subscribeShell`, `subscribeThread`, `getArchivedShellSnapshot`, `searchThreads` |
//! | [`http`] | `orchestration/http.ts`: `GET /api/orchestration/{snapshot,shell,threads/:id}`, `POST /api/orchestration/dispatch` |
//! | [`reads`] | the `zc_ports::ProjectionReads` implementation |
//! | [`pull_requests`], [`attachments`] | the parts of shared `threadPullRequests.ts` and `attachmentStore.ts` these need |
//!
//! Wiring: the engine (zc-orchestration) calls [`ProjectionPipeline::project_event_deferred`]
//! inside its dispatch transaction and runs the returned [`AttachmentCleanup`] after commit;
//! [`ProjectionPipeline::bootstrap`] runs once at startup before the engine loads its read
//! model from [`ProjectionSnapshotQuery::get_command_read_model`].
//!
//! Verification (`tests/`): `live_gates` rebuilds the projections of a copy of the live
//! database with Rust and with the TS pipeline (run from source by
//! `code/apps/server/scripts/projections-oracle.ts`) and compares every row, then compares
//! every snapshot query; `scenarios` replays made-up event logs through both, by bootstrap and
//! event by event, and compares tables, attachment files and query answers; `pipeline`,
//! `subscriptions` and `http` are the node-free ports.
//!
//! Known differences from TS:
//! - Activity payloads are re-serialized by serde_json: integer-like object keys keep their
//!   position (JS hoists them), integers above 2^53 keep their precision, lone surrogates do
//!   not decode. A question tool input without a string question is written as `{}`; TS
//!   builds `{question: undefined}`, which its own JSON codec then refuses to encode.
//! - The live budget releases a delivered batch on the RPC layer's first poll after a
//!   `Pending` (it polls again only once the client acknowledged), and on overflow releases
//!   everything but that batch at once.
//! - A completion marker is queued behind every event the pub/sub already holds (TS relies on
//!   fiber scheduling for that).
//! - `HttpApiDecodeError` bodies carry no `issues`; `cause` of `OrchestrationGetSnapshotError`
//!   is `{name: <error tag>, message}`.
//! - `inferImageExtension`'s MIME registry fallback is reduced to the image types it can map
//!   into the safe list.

pub mod activity_payload;
pub mod attachments;
pub mod budget;
pub mod coalescer;
pub mod cursor;
pub mod engine;
pub mod event;
pub mod http;
pub mod js;
pub mod pipeline;
pub mod pull_requests;
pub mod query;
pub mod reads;
pub mod subscriptions;

pub use event::ProjectionEvent;
pub use pipeline::{projector_names, AttachmentCleanup, ProjectionPipeline, Projector};
pub use query::{FixedRepositoryIdentities, NoRepositoryIdentities, NoThreadLiveState, ProjectionSnapshotQuery, RepositoryIdentityResolver, ThreadLiveState};
