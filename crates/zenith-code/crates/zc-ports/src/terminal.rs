//! The terminal port (`apps/server/src/terminal/Manager.ts` `TerminalManager`).
//!
//! Implemented by zc-terminal; consumed by the `terminal.*` RPC handlers, the setup-script
//! runner (zc-project), the thread deletion / archive / settle paths (zc-orchestration) and
//! port discovery (zc-preview).
//!
//! TS hands callbacks to `attachStream`, `subscribe` and `subscribeMetadata` and gets an
//! unsubscribe function back; here those return streams, and dropping the stream unsubscribes.

use async_trait::async_trait;

use crate::contracts::{
    TerminalAttachInput, TerminalAttachStreamEvent, TerminalClearInput, TerminalCloseInput, TerminalError, TerminalEvent, TerminalMetadataStreamEvent,
    TerminalOpenInput, TerminalResizeInput, TerminalRestartInput, TerminalSessionSnapshot, TerminalWriteInput, ThreadId,
};
use crate::EventStream;

#[async_trait]
pub trait TerminalManager: Send + Sync {
    /// `open(input)`: open or reuse the `(threadId, terminalId)` session; restores persisted
    /// history on first open.
    async fn open(&self, input: TerminalOpenInput) -> Result<TerminalSessionSnapshot, TerminalError>;

    /// `attachStream(input, listener)`: the session's snapshot first, then its live events.
    /// The RPC layer applies the windowed acks (8 chunks / 64 KiB) on top.
    async fn attach(&self, input: TerminalAttachInput) -> Result<EventStream<TerminalAttachStreamEvent>, TerminalError>;

    /// `write(input)`.
    async fn write(&self, input: TerminalWriteInput) -> Result<(), TerminalError>;

    /// `resize(input)`.
    async fn resize(&self, input: TerminalResizeInput) -> Result<(), TerminalError>;

    /// `clear(input)`: clear the output history.
    async fn clear(&self, input: TerminalClearInput) -> Result<(), TerminalError>;

    /// `restart(input)`: reset history and respawn in place.
    async fn restart(&self, input: TerminalRestartInput) -> Result<TerminalSessionSnapshot, TerminalError>;

    /// `close(input)`: one terminal, or every terminal of the thread when `terminalId` is absent.
    async fn close(&self, input: TerminalCloseInput) -> Result<(), TerminalError>;

    /// `closeIdle({threadId, terminalId?})`: close the thread's terminals that sit at an idle
    /// prompt (a terminal running a command stays open). Never fails.
    async fn close_idle(&self, thread_id: &ThreadId, terminal_id: Option<&str>);

    /// `subscribe(listener)`: every terminal runtime event from now on.
    fn subscribe(&self) -> EventStream<TerminalEvent>;

    /// `subscribeMetadata(listener)`: a full metadata snapshot first, then changes.
    fn subscribe_metadata(&self) -> EventStream<TerminalMetadataStreamEvent>;
}
