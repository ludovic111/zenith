//! The terminal RPC methods of `ws.ts` (`WS_METHODS.terminal*`, `subscribeTerminalEvents`,
//! `subscribeTerminalMetadata`), registered on a [`zc_rpc::RpcRouterBuilder`] over the port.
//!
//! | Method | Kind | Payload | Success | Ack window |
//! |---|---|---|---|---|
//! | `terminal.open` | unary | `TerminalOpenInput` | `TerminalSessionSnapshot` | |
//! | `terminal.attach` | stream | `TerminalAttachInput` | `TerminalAttachStreamEvent` | 8 chunks / 64 KiB |
//! | `terminal.write` | unary | `TerminalWriteInput` (data ≤ 65,536) | void | |
//! | `terminal.resize` | unary | `TerminalResizeInput` | void | |
//! | `terminal.clear` | unary | `TerminalClearInput` | void | |
//! | `terminal.restart` | unary | `TerminalRestartInput` | `TerminalSessionSnapshot` | |
//! | `terminal.close` | unary | `TerminalCloseInput` | void | |
//! | `subscribeTerminalEvents` | stream | `{}` | `TerminalEvent` | 8 chunks / 64 KiB |
//! | `subscribeTerminalMetadata` | stream | `{}` | `TerminalMetadataStreamEvent` | one chunk |
//!
//! Every method needs the `terminal:operate` scope. Only `terminal.attach` and
//! `subscribeTerminalEvents` get the windowed acks (`withTerminalOutputWindow` matches exactly
//! those two tags; `OutputProtocol.test.ts` checks that metadata stays at one chunk). The
//! streams never end on their own; the client interrupts them.

use std::sync::Arc;

use futures::StreamExt;
use zc_ports::contracts as wire;
use zc_ports::TaggedError;
use zc_rpc::{AckWindow, Failure, MethodOptions, RpcMethod, RpcRouterBuilder, ScopeRule};

use crate::contracts::{
    EmptyInput, TerminalAttachInput, TerminalClearInput, TerminalCloseInput, TerminalOpenInput, TerminalResizeInput, TerminalRestartInput, TerminalWriteInput,
};

/// The scope of every terminal method (`RPC_REQUIRED_SCOPES`).
pub const TERMINAL_OPERATE_SCOPE: &str = "terminal:operate";

/// Every method this module registers, with whether it streams.
pub const TERMINAL_METHODS: [(&str, bool); 9] = [
    ("terminal.open", false),
    ("terminal.attach", true),
    ("terminal.write", false),
    ("terminal.resize", false),
    ("terminal.clear", false),
    ("terminal.restart", false),
    ("terminal.close", false),
    ("subscribeTerminalEvents", true),
    ("subscribeTerminalMetadata", true),
];

macro_rules! method {
    ($name:ident, $tag:literal, $stream:literal, $payload:ty, $success:ty) => {
        #[doc = concat!("`", $tag, "`.")]
        pub struct $name;
        impl RpcMethod for $name {
            const TAG: &'static str = $tag;
            const STREAM: bool = $stream;
            type Payload = $payload;
            type Success = $success;
            type Error = TaggedError;
        }
    };
}

method!(TerminalOpen, "terminal.open", false, TerminalOpenInput, wire::TerminalSessionSnapshot);
method!(TerminalAttach, "terminal.attach", true, TerminalAttachInput, wire::TerminalAttachStreamEvent);
method!(TerminalWrite, "terminal.write", false, TerminalWriteInput, ());
method!(TerminalResize, "terminal.resize", false, TerminalResizeInput, ());
method!(TerminalClear, "terminal.clear", false, TerminalClearInput, ());
method!(TerminalRestart, "terminal.restart", false, TerminalRestartInput, wire::TerminalSessionSnapshot);
method!(TerminalClose, "terminal.close", false, TerminalCloseInput, ());
method!(SubscribeTerminalEvents, "subscribeTerminalEvents", true, EmptyInput, wire::TerminalEvent);
method!(
    SubscribeTerminalMetadata,
    "subscribeTerminalMetadata",
    true,
    EmptyInput,
    wire::TerminalMetadataStreamEvent
);

fn to_value<T: serde::Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).expect("terminal inputs always encode")
}

fn options(window: AckWindow) -> MethodOptions {
    MethodOptions::default().scope(ScopeRule::required(TERMINAL_OPERATE_SCOPE)).ack_window(window)
}

/// Registers the nine terminal methods.
pub fn register(builder: RpcRouterBuilder, terminals: Arc<dyn zc_ports::TerminalManager>) -> RpcRouterBuilder {
    let unary = options(AckWindow::PER_CHUNK);
    let t = terminals.clone();
    let builder = builder.typed_unary_with::<TerminalOpen, _, _>(unary.clone(), move |_, input| {
        let t = t.clone();
        async move { Ok(t.open(wire::TerminalOpenInput(to_value(&input))).await?) }
    });
    let t = terminals.clone();
    let builder = builder.typed_stream_with::<TerminalAttach, _, _, _>(options(AckWindow::TERMINAL), move |_, input| {
        let t = t.clone();
        async move {
            let stream = t.attach(wire::TerminalAttachInput(to_value(&input))).await?;
            Ok(stream.map(Ok::<_, Failure<TaggedError>>))
        }
    });
    let t = terminals.clone();
    let builder = builder.typed_unary_with::<TerminalWrite, _, _>(unary.clone(), move |_, input| {
        let t = t.clone();
        async move { Ok(t.write(wire::TerminalWriteInput(to_value(&input))).await?) }
    });
    let t = terminals.clone();
    let builder = builder.typed_unary_with::<TerminalResize, _, _>(unary.clone(), move |_, input| {
        let t = t.clone();
        async move { Ok(t.resize(wire::TerminalResizeInput(to_value(&input))).await?) }
    });
    let t = terminals.clone();
    let builder = builder.typed_unary_with::<TerminalClear, _, _>(unary.clone(), move |_, input| {
        let t = t.clone();
        async move { Ok(t.clear(wire::TerminalClearInput(to_value(&input))).await?) }
    });
    let t = terminals.clone();
    let builder = builder.typed_unary_with::<TerminalRestart, _, _>(unary.clone(), move |_, input| {
        let t = t.clone();
        async move { Ok(t.restart(wire::TerminalRestartInput(to_value(&input))).await?) }
    });
    let t = terminals.clone();
    let builder = builder.typed_unary_with::<TerminalClose, _, _>(unary.clone(), move |_, input| {
        let t = t.clone();
        async move { Ok(t.close(wire::TerminalCloseInput(to_value(&input))).await?) }
    });
    let t = terminals.clone();
    let builder = builder.typed_stream_with::<SubscribeTerminalEvents, _, _, _>(options(AckWindow::TERMINAL), move |_, _input| {
        let t = t.clone();
        async move { Ok(t.subscribe().map(Ok::<_, Failure<TaggedError>>)) }
    });
    let t = terminals;
    builder.typed_stream_with::<SubscribeTerminalMetadata, _, _, _>(options(AckWindow::PER_CHUNK), move |_, _input| {
        let t = t.clone();
        async move { Ok(t.subscribe_metadata().map(Ok::<_, Failure<TaggedError>>)) }
    })
}
