//! zc-rpc: the Effect RPC server runtime over WebSocket, as the zenith code web client
//! speaks it (`effect/unstable/rpc` with `RpcSerialization.layerJson`, effect rc.115).
//! See `docs/zenith-code-rust-plan.md` §1.3–1.4.
//!
//! - [`message`]: frame decoding and the server envelopes (`Chunk`, `Exit`, `Defect`,
//!   `Pong`); request ids are echoed with their JSON type.
//! - [`exit`]: `Exit`/`Cause` encoded exactly as Effect encodes them.
//! - [`router`]: handlers registered by tag, over raw JSON or typed ([`typed`]), each
//!   with a scope rule (from a [`ScopeTable`] or per method) and an [`AckWindow`].
//! - [`server`]: [`RpcServer::serve_socket`] runs one socket: one task per request,
//!   `Ack` backpressure for streams, `Interrupt` → cancellation, immediate `Pong`,
//!   unknown tags and bad payloads → a per-request `Exit` with `Die`; only frames that
//!   cannot be decoded at all get a connection-wide `Defect`.
//!
//! The transport is abstract ([`Inbound`]/[`Outbound`] over any `Stream`/`Sink`); zc-http
//! plugs axum's WebSocket into it.

pub mod context;
pub mod error;
pub mod exit;
pub mod message;
pub mod router;
pub mod server;
pub mod typed;

pub use context::{AuthContext, ConnectionInfo, RequestContext};
pub use error::{authorization_error, RpcError};
pub use exit::{CauseReason, Exit};
pub use message::RequestId;
pub use router::{AckWindow, BoxValueStream, MethodOptions, RouterError, RpcRouter, RpcRouterBuilder, ScopeRule, ScopeTable};
pub use server::{CloseListener, ConnectionSetup, Inbound, Outbound, RpcServer, ServerOptions};
pub use tokio_util::sync::CancellationToken;
pub use typed::{Failure, RpcMethod};
