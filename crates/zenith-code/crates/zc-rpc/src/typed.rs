//! Typed methods, for when zc-contracts generates one type per RPC.
//!
//! zc-contracts is meant to emit, per method of `WsRpcGroup`, a marker type with the
//! method's tag, kind and encoded types. Until it exists, the trait lives here and has
//! no dependency beyond serde, so the generated code can implement it directly (or
//! zc-rpc can re-export the contracts' own trait in its place: same shape).
//!
//! ```ignore
//! pub struct ServerProbe;
//! impl zc_rpc::RpcMethod for ServerProbe {
//!     const TAG: &'static str = "server.probe";
//!     const STREAM: bool = false;
//!     type Payload = ServerProbeInput;
//!     type Success = ServerProbeResult;
//!     type Error = ServerProbeError; // the method's error union, `#[serde(tag = "_tag")]`
//! }
//!
//! RpcRouter::builder()
//!     .scopes(zc_contracts::rpc::SCOPES.iter().copied().collect())
//!     .typed_unary::<ServerProbe, _, _>(|ctx, input| async move { … })
//! ```

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use crate::error::RpcError;
use crate::router::{DecodeFn, ErasedPayload};

/// One RPC of the group: tag, kind, and the types that cross the wire.
pub trait RpcMethod: Send + Sync + 'static {
    const TAG: &'static str;
    /// True for stream methods: `Success` is then the item type and the final `Exit`
    /// carries `null`.
    const STREAM: bool;
    type Payload: DeserializeOwned + Send + 'static;
    type Success: Serialize + Send + 'static;
    /// The method's error union (for streams, including the stream error schema).
    type Error: Serialize + Send + 'static;
}

/// A failed typed call: a typed error (`Fail`), a defect (`Die`) or an interruption.
///
/// `From<E>` lets handlers use `?` on their typed errors.
#[derive(Debug)]
pub enum Failure<E> {
    Fail(E),
    Die(Value),
    Interrupt,
}

impl<E> From<E> for Failure<E> {
    fn from(error: E) -> Self {
        Self::Fail(error)
    }
}

impl<E> Failure<E> {
    /// A defect shaped like a JS `Error`.
    pub fn die(message: impl std::fmt::Display) -> Self {
        match RpcError::die(message) {
            RpcError::Die(defect) => Self::Die(defect),
            _ => unreachable!(),
        }
    }
}

impl<E: Serialize> Failure<E> {
    pub fn into_rpc_error(self) -> RpcError {
        match self {
            Self::Fail(error) => RpcError::fail(error),
            Self::Die(defect) => RpcError::Die(defect),
            Self::Interrupt => RpcError::Interrupt,
        }
    }
}

pub(crate) fn encode_success<T: Serialize>(value: &T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|e| RpcError::die(format!("could not encode the result: {e}")))
}

/// Decodes the payload before the scope check, like the TS server (decode failures are
/// a per-request `Die` whose defect is the error text).
pub(crate) fn decode_fn<P: DeserializeOwned + Send + 'static>() -> DecodeFn {
    std::sync::Arc::new(|value: Value| {
        serde_json::from_value::<P>(value)
            .map(|p| Box::new(p) as ErasedPayload)
            .map_err(|e| e.to_string())
    })
}
