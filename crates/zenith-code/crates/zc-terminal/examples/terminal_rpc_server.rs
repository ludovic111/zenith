//! Serves real terminals over Effect RPC on a WebSocket, with only the terminal methods:
//!
//!   cargo run -p zc-terminal --example terminal_rpc_server -- [127.0.0.1:3790] [logs dir]
//!
//! Then point any Effect RPC client using the contracts' `WsRpcGroup` at
//! `ws://127.0.0.1:3790/ws` (no auth: local use only). Ctrl-C stops the terminals.

#[path = "../tests/support/server.rs"]
mod server;

use std::sync::Arc;

use zc_terminal::{PortablePtyAdapter, TerminalManager, TerminalManagerOptions};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let bind = args.next().unwrap_or_else(|| "127.0.0.1:3790".into());
    let logs_dir = args
        .next()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("zc-terminal-example/logs/terminals"));
    let terminals = TerminalManager::new(TerminalManagerOptions::new(&logs_dir, Arc::new(PortablePtyAdapter))).await?;
    let server = server::serve_terminals(Arc::new(terminals.clone()), &bind).await?;
    println!("terminal RPC on {} (history in {})", server.ws_url(), logs_dir.display());
    let _ = tokio::signal::ctrl_c().await;
    server.stop().await;
    terminals.shutdown().await;
    Ok(())
}
