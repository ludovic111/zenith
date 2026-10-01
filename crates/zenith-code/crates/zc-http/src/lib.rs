//! zc-http: the HTTP pieces of the zenith code server that do not depend on any service
//! (plan §1.1–1.2, §2.6–2.7). The route table itself is assembled in
//! `zenith-code::server`.
//!
//! - [`static_files`]: the SPA with index fallback, caching rules and the frame-ancestors
//!   CSP on HTML only;
//! - [`embed`]: `ZENITH_CODE_PARENT_ORIGINS` and `GET /zenith/embed.json`;
//! - [`cors`]: Effect's CORS middleware as the TS server configures it;
//! - [`readiness`]: the command readiness gate;
//! - [`error`]: typed JSON tagged-error responses with declared statuses;
//! - [`ws`]: the `/ws` upgrade, its query parameters, the [`ws::WsAuthenticator`] auth
//!   hook, and the glue from axum's socket to [`zc_rpc::RpcServer`].

pub mod cors;
pub mod embed;
pub mod error;
pub mod readiness;
pub mod static_files;
pub mod ws;

pub use cors::CorsPolicy;
pub use error::{new_trace_id, EnvironmentError, TaggedError};
pub use readiness::{Readiness, ReadinessGate};
pub use static_files::StaticSite;
pub use ws::{DevAuthenticator, OriginCheck, UpgradeRequest, WsAuthenticator, WsQuery, WsState};
