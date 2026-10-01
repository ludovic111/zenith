//! The typed client over the peer (`effect-codex-app-server/src/client.ts`): requests encode
//! their params and decode their response against the protocol types; a response that does not
//! decode is a `-32602` "Invalid payload" request error, as in TS. `raw` requests skip both,
//! for the calls TS makes through `client.raw.request`.

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::Value;
use zc_codex_protocol::ClientRequest;

use crate::errors::{CodexAppServerError, RequestError, RequestOperation};
use crate::peer::CodexPeer;

/// What the history and thread-opening helpers need from a client; tests fake it.
#[async_trait]
pub trait CodexRequester: Send + Sync {
    /// A request whose params and response are passed as they are.
    async fn request_raw(&self, method: &str, params: Option<Value>) -> Result<Value, CodexAppServerError>;
}

#[async_trait]
impl CodexRequester for CodexPeer {
    async fn request_raw(&self, method: &str, params: Option<Value>) -> Result<Value, CodexAppServerError> {
        self.request(method, params).await
    }
}

/// Decodes a response as `T`, mapping a mismatch to TS's `invalidPayload`.
pub fn decode_response<T: DeserializeOwned>(method: &str, value: Value) -> Result<T, CodexAppServerError> {
    serde_json::from_value(value)
        .map_err(|error| CodexAppServerError::Request(RequestError::invalid_payload(method, RequestOperation::DecodePayload, &error.to_string())))
}

/// A typed request: encodes `params`, decodes the response.
pub async fn request<M: ClientRequest>(client: &(impl CodexRequester + ?Sized), params: &M::Params) -> Result<M::Response, CodexAppServerError> {
    let encoded = serde_json::to_value(params)
        .map_err(|error| CodexAppServerError::Request(RequestError::invalid_payload(M::METHOD, RequestOperation::EncodePayload, &error.to_string())))?;
    // A method without params sends no `params` key; a nullable one sends `null`, as TS does.
    let encoded = M::HAS_PARAMS.then_some(encoded);
    let response = client.request_raw(M::METHOD, encoded).await?;
    decode_response(M::METHOD, response)
}
