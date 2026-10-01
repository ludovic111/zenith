//! [`zc_ports::TerminalManager`] for [`TerminalManager`]: the typed manager behind the port's
//! wire-shaped placeholders. Inputs are decoded with the contract types (so the schema
//! checks apply to internal callers too); outputs and errors are encoded once.

use async_trait::async_trait;
use futures::StreamExt;
use serde::de::DeserializeOwned;
use serde::Serialize;
use zc_ports::contracts as wire;
use zc_ports::{EventStream, TaggedError};

use crate::contracts::{
    TerminalAttachInput, TerminalClearInput, TerminalCloseInput, TerminalOpenInput, TerminalResizeInput, TerminalRestartInput, TerminalWriteInput,
};
use crate::manager::TerminalManager;

/// The tag of an input the port could not decode. It is not part of `TerminalError`: the RPC
/// handlers decode payloads before calling the port, so only a malformed internal call can
/// produce it.
pub const INVALID_INPUT_TAG: &str = "TerminalInvalidInputError";

fn decode<T: DeserializeOwned>(value: serde_json::Value) -> Result<T, TaggedError> {
    serde_json::from_value(value).map_err(|error| TaggedError::new(INVALID_INPUT_TAG, error.to_string()).with("message", error.to_string()))
}

fn encode<T: Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).expect("terminal contract values always encode")
}

#[async_trait]
impl zc_ports::TerminalManager for TerminalManager {
    async fn open(&self, input: wire::TerminalOpenInput) -> Result<wire::TerminalSessionSnapshot, TaggedError> {
        let input: TerminalOpenInput = decode(input.0)?;
        let snapshot = TerminalManager::open(self, input).await?;
        Ok(wire::TerminalSessionSnapshot(encode(&snapshot)))
    }

    async fn attach(&self, input: wire::TerminalAttachInput) -> Result<EventStream<wire::TerminalAttachStreamEvent>, TaggedError> {
        let input: TerminalAttachInput = decode(input.0)?;
        let stream = TerminalManager::attach_stream(self, input).await?;
        Ok(stream.map(|event| wire::TerminalAttachStreamEvent(encode(&event))).boxed())
    }

    async fn write(&self, input: wire::TerminalWriteInput) -> Result<(), TaggedError> {
        let input: TerminalWriteInput = decode(input.0)?;
        Ok(TerminalManager::write(self, input).await?)
    }

    async fn resize(&self, input: wire::TerminalResizeInput) -> Result<(), TaggedError> {
        let input: TerminalResizeInput = decode(input.0)?;
        Ok(TerminalManager::resize(self, input).await?)
    }

    async fn clear(&self, input: wire::TerminalClearInput) -> Result<(), TaggedError> {
        let input: TerminalClearInput = decode(input.0)?;
        Ok(TerminalManager::clear(self, input).await?)
    }

    async fn restart(&self, input: wire::TerminalRestartInput) -> Result<wire::TerminalSessionSnapshot, TaggedError> {
        let input: TerminalRestartInput = decode(input.0)?;
        let snapshot = TerminalManager::restart(self, input).await?;
        Ok(wire::TerminalSessionSnapshot(encode(&snapshot)))
    }

    async fn close(&self, input: wire::TerminalCloseInput) -> Result<(), TaggedError> {
        let input: TerminalCloseInput = decode(input.0)?;
        Ok(TerminalManager::close(self, input).await?)
    }

    async fn close_idle(&self, thread_id: &wire::ThreadId, terminal_id: Option<&str>) {
        TerminalManager::close_idle(self, thread_id.as_str(), terminal_id).await;
    }

    fn subscribe(&self) -> EventStream<wire::TerminalEvent> {
        TerminalManager::subscribe(self).map(|event| wire::TerminalEvent(encode(&event))).boxed()
    }

    fn subscribe_metadata(&self) -> EventStream<wire::TerminalMetadataStreamEvent> {
        TerminalManager::subscribe_metadata(self)
            .map(|event| wire::TerminalMetadataStreamEvent(encode(&event)))
            .boxed()
    }
}
