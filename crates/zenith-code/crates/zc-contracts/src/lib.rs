//! zenith code's wire contracts, generated from the Effect schemas of `code/packages/contracts`.
//!
//! Every type models the **encoded** side of a schema (what `Schema.toCodecJson` writes and
//! reads), so a value serialized with `serde_json` is exactly what the web client expects.
//!
//! - The types keep the export names of the TypeScript contracts (`ThreadId`, `ServerConfig`,
//!   `OrchestrationEvent`, …). Anonymous structs and unions are named after their parent and
//!   field (`ServerConfigSettings`), union members after their tag (`OrchestrationEventThreadCreated`).
//! - [`rpc_methods`]: the 148 WebSocket RPC methods ([`Rpc`], [`METHODS`], [`RpcMethod`],
//!   [`methods`], [`zc_for_each_rpc!`]).
//! - [`http_endpoints`]: the 24 typed HTTP endpoints ([`ENDPOINTS`], [`HttpEndpoint`],
//!   [`endpoints`]).
//! - [`prim`]: the hand-written primitives ([`JsNumber`], [`DateTimeUtc`], [`EOption`], …).
//!
//! Regenerate with `node code/scripts/gen-rust-contracts.ts` (see docs/zenith-code/contracts.md).

pub mod prim;

mod generated;

pub use generated::*;
pub use prim::{double_option, lenient_double_option, lenient_option, Base64Bytes, DateTimeUtc, EOption, JsNumber, LenientVec, Never};

#[cfg(test)]
mod tests;
