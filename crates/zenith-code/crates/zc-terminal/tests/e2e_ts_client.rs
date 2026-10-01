//! The real TS client (`effect/unstable/rpc` + the contracts' `WsRpcGroup`) against the Rust
//! terminal handlers, through zc-rpc on a real WebSocket and a real PTY
//! (`tests/ts/terminal-e2e.mjs`).
//!
//! Skipped (with a note) when `node` or the TS dependencies are missing: it needs
//! `code/packages/contracts/node_modules/effect` (after `pnpm install` in `code/`).

#![cfg(unix)]

#[path = "support/server.rs"]
mod server;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use zc_terminal::{PortablePtyAdapter, TerminalManager, TerminalManagerOptions};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../..").canonicalize().unwrap()
}

/// A scratch directory with the script and a `node_modules/effect` link to the very `effect`
/// the contracts use (one module instance, so their schemas and the client agree).
fn prepare_script_dir(effect: &Path) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("node_modules")).unwrap();
    std::os::unix::fs::symlink(effect, dir.path().join("node_modules/effect")).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ts/terminal-e2e.mjs"),
        dir.path().join("terminal-e2e.mjs"),
    )
    .unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_ts_client_attaches_and_types_with_windowed_acks() {
    if std::process::Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipped: node is not installed");
        return;
    }
    let contracts = repo_root().join("code/packages/contracts");
    let Ok(effect) = contracts.join("node_modules/effect").canonicalize() else {
        eprintln!("skipped: code/packages/contracts/node_modules/effect is missing (pnpm install in code/)");
        return;
    };
    let script_dir = prepare_script_dir(&effect);

    let work = tempfile::tempdir().unwrap();
    let cwd = work.path().canonicalize().unwrap().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut options = TerminalManagerOptions::new(work.path().join("logs/terminals"), Arc::new(PortablePtyAdapter));
    options.shell_resolver = Some(Arc::new(|| "/bin/sh".into()));
    let mut env = zc_terminal::shell::process_env();
    env.insert("PS1".into(), "$ ".into());
    options.env = Some(env);
    options.process_kill_grace = Duration::from_millis(200);
    let terminals = TerminalManager::new(options).await.unwrap();
    let server = server::serve_terminals(Arc::new(terminals.clone()), "127.0.0.1:0").await.unwrap();

    let output = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new("node")
            .arg(script_dir.path().join("terminal-e2e.mjs"))
            .arg(server.ws_url())
            .arg(contracts.join("src/rpc.ts"))
            .arg(&cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .expect("the TS client finished in time")
    .unwrap();
    println!("{}", String::from_utf8_lossy(&output.stdout));
    eprintln!("{}", String::from_utf8_lossy(&output.stderr));

    server.stop().await;
    terminals.shutdown().await;
    assert!(output.status.success(), "end-to-end checks failed");
}
