//! zenith code's server (see docs/zenith-code-rust-plan.md), assembled here as the crates
//! under crates/zenith-code/crates/ come in. The command line is in [`zenith_code::cli`]:
//! `serve` (the server, see [`zenith_code::serve`] and [`zenith_code::app`]),
//! `auth pairing|session …`, `project add|remove|rename`, and `dev-serve`.
//!
//! `dev-serve [--host 127.0.0.1] [--port 0] [--static-dir DIR] [--conformance]` is a development
//! server with no real authentication (every socket gets fixed scopes), so it only ever listens
//! on what `--host` says, 127.0.0.1 by default. `--conformance` serves the dummy RPC group of
//! `code/scripts/rpc-conformance`. Without `--static-dir`, pages come from
//! `ZENITH_CODE_STATIC_DIR` or this repository's `code/apps/server/dist/client`. It prints
//! `listening on http://ADDR` once ready.
//!
//! `GET /api/zenith/sessions` (zc-sessions) reads this Mac's Claude Code and Codex logs and
//! maps sessions to the projects of `--projects`, a JSON array of
//! `{"id","title","workspaceRoot"}` (none by default).

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use zc_http::{DevAuthenticator, ReadinessGate};
use zc_rpc::{RpcRouter, RpcServer};
use zenith_code::cli::{self, Command, DevServeArgs};
use zenith_code::server::{self, conformance, AppServices, HttpConfig};

fn main() -> ExitCode {
    let cli = match cli::parse() {
        Ok(cli) => cli,
        Err(code) => return code,
    };
    // Machine-readable auth output keeps the logs to errors, like the TS CLI's `quietLogs`.
    let default_filter = if cli.quiet_logs() { "error" } else { "info" };
    // Console logs on stderr, plus the trace file layer `serve` points at logs/server.trace.ndjson.
    zc_telemetry::logging::init(default_filter);

    // `fixPath`: the login shell's PATH (GUI launches inherit a minimal one), set before any
    // thread exists since changing the environment is not thread-safe.
    if matches!(cli.command, Command::Serve(_)) {
        zc_core::shell_env::fix_path().apply_to_process();
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Error: {error}");
            return ExitCode::from(1);
        }
    };
    let log_level = cli.log_level.clone();
    let result = runtime.block_on(async move {
        match cli.command {
            Command::Serve(args) => zenith_code::serve::serve(args, log_level.as_deref()).await,
            Command::Auth(args) => cli::run_auth(args.command, log_level.as_deref()).await,
            Command::Project(args) => zenith_code::project_cli::run_project(args.command, log_level.as_deref()).await,
            Command::DevServe(args) => dev_serve(args).await,
        }
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:#}");
            ExitCode::from(1)
        }
    }
}

async fn dev_serve(args: DevServeArgs) -> anyhow::Result<()> {
    let (router, scopes) = if args.conformance {
        (conformance::router(), conformance::scopes())
    } else {
        (RpcRouter::builder().build()?, Vec::new())
    };
    let rpc = RpcServer::new(router);
    let readiness = ReadinessGate::new();
    let services = AppServices {
        rpc: rpc.clone(),
        auth: Arc::new(DevAuthenticator { scopes }),
        readiness: readiness.clone(),
    };
    let projects: Vec<zc_sessions::ProjectRoot> = match &args.projects {
        Some(file) => serde_json::from_slice(&std::fs::read(file).with_context(|| format!("cannot read {}", file.display()))?)
            .with_context(|| format!("{}: expected [{{id, title, workspaceRoot}}]", file.display()))?,
        None => Vec::new(),
    };
    let sessions = zc_sessions::router(zc_sessions::SessionsApi {
        reader: Arc::new(zc_sessions::SessionReader::new(zc_sessions::ReaderConfig::from_env())),
        projects: Arc::new(zc_sessions::StaticProjectRoots(projects)),
        // No real auth here either: the route gets the one scope it needs.
        auth: Arc::new(zc_sessions::WsAuthBridge(Arc::new(DevAuthenticator {
            scopes: vec![zc_sessions::REQUIRED_SCOPE.to_owned()],
        }))),
    });
    let app = server::router_with(&HttpConfig::from_env(args.static_dir.or_else(server::default_static_dir)), &services, sessions);

    let listener = tokio::net::TcpListener::bind((args.host.as_str(), args.port))
        .await
        .with_context(|| format!("cannot listen on {}:{}", args.host, args.port))?;
    let addr = listener.local_addr()?;
    // Nothing to start up yet: open the gate at once.
    readiness.mark_ready();
    println!("listening on http://{addr}");
    std::io::stdout().flush()?;
    server::serve(listener, app, rpc, server::shutdown_signal()).await?;
    Ok(())
}
