//! `serve`: the real server (plan §6.17–6.18), as zenith.app's LaunchAgent and the dashboards
//! run it (`serve --host 127.0.0.1 --port 4747 --base-dir ~/.zenith/code`).
//!
//! 1. resolve the config (`serve` flags over `T3CODE_*` over defaults); the login-shell `PATH`
//!    fix already ran in `main`, before the runtime started;
//! 2. [`App::build`]: every service, the RPC and HTTP routers (see [`crate::app`]);
//! 3. bind the listener and start serving: every request waits on the command readiness gate;
//! 4. [`App::startup`]: keybindings, settings, reactors, reconcilers, auto-pull, the banner
//!    (only `zenith server is ready.` with `ZENITH_NO_STARTUP_TOKEN=1`), `welcome`,
//!    `server-runtime.json`, the gate opens, `ready`;
//! 5. on SIGTERM / Ctrl-C (or a startup failure): close the sockets, finish in-flight requests,
//!    stop plugins, providers and terminals, delete `server-runtime.json`.

use anyhow::Context;
use tokio_util::sync::CancellationToken;

use crate::app::App;
use crate::cli::{resolve_serve_config, ServeArgs};
use crate::server;

pub use crate::app::{placeholder_endpoints, register_placeholders, SERVER_VERSION};

/// `serve`.
pub async fn serve(args: ServeArgs, log: Option<&str>) -> anyhow::Result<()> {
    let config = resolve_serve_config(&args, log).await?;
    let host = config.host.clone();
    let port = config.port;
    let app = App::build(config).await?;

    let bind_host = host.unwrap_or_else(|| "0.0.0.0".to_owned());
    let listener = tokio::net::TcpListener::bind((bind_host.as_str(), port))
        .await
        .with_context(|| format!("cannot listen on {bind_host}:{port}"))?;
    let port = listener.local_addr()?.port();

    let stop = CancellationToken::new();
    let serving = tokio::spawn(server::serve(listener, app.router.clone(), app.rpc.clone(), {
        let stop = stop.clone();
        async move {
            tokio::select! {
                _ = server::shutdown_signal() => {}
                _ = stop.cancelled() => {}
            }
        }
    }));

    let startup = app.startup(port).await;
    if startup.is_err() {
        stop.cancel();
    }
    let served = serving.await.context("server task failed")?;
    app.shutdown().await;
    startup?;
    served.context("server error")
}
