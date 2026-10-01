//! A WebSocket server with only the terminal RPC methods, for the end-to-end test and the
//! `terminal_rpc_server` example. Every socket gets the `terminal:operate` scope
//! (`DevAuthenticator`): local use only.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::routing::get;
use axum::Router;
use zc_http::ws::ws_upgrade;
use zc_http::{DevAuthenticator, WsState};
use zc_rpc::{RpcRouter, RpcServer};
use zc_terminal::rpc::{register, TERMINAL_OPERATE_SCOPE};

/// A running server.
pub struct TerminalRpcServer {
    pub addr: SocketAddr,
    pub rpc: Arc<RpcServer>,
    task: tokio::task::JoinHandle<()>,
}

impl TerminalRpcServer {
    pub fn ws_url(&self) -> String {
        format!("ws://{}/ws", self.addr)
    }

    /// Closes every socket (interrupting the streams) and stops listening.
    pub async fn stop(self) {
        self.rpc.shutdown().await;
        self.task.abort();
        let _ = self.task.await;
    }
}

/// Serves the terminal methods of `terminals` on `bind` (e.g. `127.0.0.1:0`).
pub async fn serve_terminals(terminals: Arc<dyn zc_ports::TerminalManager>, bind: &str) -> std::io::Result<TerminalRpcServer> {
    let router = register(RpcRouter::builder(), terminals)
        .build()
        .expect("the terminal methods register cleanly");
    let rpc = RpcServer::new(router);
    let state = WsState {
        rpc: rpc.clone(),
        auth: Arc::new(DevAuthenticator {
            scopes: vec![TERMINAL_OPERATE_SCOPE.to_owned()],
        }),
        origin_check: None,
    };
    let app = Router::new().route("/ws", get(ws_upgrade)).with_state(state);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let addr = listener.local_addr()?;
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await;
    });
    Ok(TerminalRpcServer { addr, rpc, task })
}
