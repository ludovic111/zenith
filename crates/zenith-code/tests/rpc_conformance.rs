//! The real Effect RPC client (code/scripts/rpc-conformance/conformance.mjs) against the
//! Rust server, in-process, with the conformance handlers.
//!
//! Skipped (with a note) when node or code/node_modules is missing. The ping/pong wait is
//! short here (`ZC_CONFORMANCE_PING_SECONDS`, default 6); `run.sh` does the full 32 s.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use zc_http::{DevAuthenticator, ReadinessGate};
use zc_rpc::RpcServer;
use zenith_code::server::{conformance, router, serve, AppServices, HttpConfig};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// `node_modules/effect` next to the script, linked from code/node_modules if needed.
fn ensure_effect(script_dir: &Path) -> Result<(), String> {
    let link = script_dir.join("node_modules/effect");
    if link.exists() {
        return Ok(());
    }
    let pnpm = repo_root().join("code/node_modules/.pnpm");
    let entry = std::fs::read_dir(&pnpm)
        .map_err(|_| format!("{} is missing (pnpm install in code/)", pnpm.display()))?
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().starts_with("effect@4.0.0-rc.115"))
        .ok_or("effect@4.0.0-rc.115 is not installed")?;
    std::fs::create_dir_all(script_dir.join("node_modules")).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(entry.path().join("node_modules/effect"), &link).map_err(|e| e.to_string())?;
    #[cfg(not(unix))]
    return Err("linking effect needs a unix symlink".into());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_effect_client_against_the_rust_server() {
    let script_dir = repo_root().join("code/scripts/rpc-conformance");
    if std::process::Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipped: node is not installed");
        return;
    }
    if let Err(why) = ensure_effect(&script_dir) {
        eprintln!("skipped: {why}");
        return;
    }

    let rpc = RpcServer::new(conformance::router());
    let services = AppServices {
        rpc: rpc.clone(),
        auth: Arc::new(DevAuthenticator { scopes: conformance::scopes() }),
        readiness: ReadinessGate::ready(),
    };
    let app = router(&HttpConfig::from_env(None), &services);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve(listener, app, rpc.clone(), async {
        let _ = stop_rx.await;
    }));

    let ping_seconds = std::env::var("ZC_CONFORMANCE_PING_SECONDS").unwrap_or_else(|_| "6".into());
    let output = tokio::process::Command::new("node")
        .arg(script_dir.join("conformance.mjs"))
        .arg(format!("ws://{addr}/ws"))
        .env("PING_SECONDS", ping_seconds)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .unwrap();
    println!("{}", String::from_utf8_lossy(&output.stdout));
    eprintln!("{}", String::from_utf8_lossy(&output.stderr));

    let _ = stop_tx.send(());
    tokio::time::timeout(std::time::Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(rpc.connection_count(), 0, "every socket closed on shutdown");
    assert!(output.status.success(), "conformance checks failed");
}
