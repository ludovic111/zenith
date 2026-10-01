//! What a handler returns when it does not succeed.

use serde::Serialize;
use serde_json::Value;

use crate::exit::{CauseReason, Exit};
use crate::message::error_defect;

/// A failed request, as it goes on the wire.
#[derive(Clone, Debug, PartialEq)]
pub enum RpcError {
    /// A typed failure from the method's error union, already encoded
    /// (`{"_tag":"EnvironmentAuthorizationError",…}`). Becomes `Fail`.
    Fail(Value),
    /// An unexpected error. Becomes `Die`: per request, never a connection `Defect`.
    Die(Value),
    /// The request was interrupted.
    Interrupt,
}

impl RpcError {
    /// A typed failure. If the error cannot be serialized, the request dies instead.
    pub fn fail<E: Serialize>(error: E) -> Self {
        match serde_json::to_value(error) {
            Ok(value) => Self::Fail(value),
            Err(e) => Self::die(format!("could not encode the error: {e}")),
        }
    }

    /// A defect shaped like a JS `Error`: `{"name":"Error","message":…}`.
    pub fn die(message: impl std::fmt::Display) -> Self {
        Self::Die(error_defect("Error", &message.to_string()))
    }

    /// A defect that is a bare string, like the TS server's `Unknown request tag: …` and
    /// payload decode errors.
    pub fn die_text(message: impl Into<String>) -> Self {
        Self::Die(Value::String(message.into()))
    }

    pub fn into_exit(self) -> Exit {
        match self {
            Self::Fail(error) => Exit::Failure(vec![CauseReason::Fail(error)]),
            Self::Die(defect) => Exit::Failure(vec![CauseReason::Die(defect)]),
            Self::Interrupt => Exit::interrupt(),
        }
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fail(error) => write!(f, "failure {error}"),
            Self::Die(defect) => write!(f, "defect {defect}"),
            Self::Interrupt => f.write_str("interrupted"),
        }
    }
}

impl std::error::Error for RpcError {}

/// The error the TS server returns when the session lacks a method's scope
/// (`ws.ts` `authorizationError`): `{"_tag":"EnvironmentAuthorizationError","message":…,
/// "requiredScope":…}`.
pub fn authorization_error(required_scope: &str) -> RpcError {
    let mut map = serde_json::Map::new();
    map.insert("_tag".into(), "EnvironmentAuthorizationError".into());
    map.insert(
        "message".into(),
        format!("The authenticated token is missing required scope: {required_scope}.").into(),
    );
    map.insert("requiredScope".into(), required_scope.into());
    RpcError::Fail(Value::Object(map))
}

/// Text of a panic payload, for the `Die` defect of a handler that panicked.
pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "handler panicked".to_owned()
    }
}
