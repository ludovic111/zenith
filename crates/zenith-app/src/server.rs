//! The local zenith server (zenith code): where it answers, whether it does, waking it up,
//! and its command line. It runs on its own (a LaunchAgent, see scripts/mac/install.sh), so
//! the app can close and the agents keep working.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

/// `ZENITH_URL` at run time (a development build), else what install.sh built the app with.
pub fn base() -> String {
    std::env::var("ZENITH_URL")
        .ok()
        .or(option_env!("ZENITH_URL").map(String::from))
        .unwrap_or_else(|| "http://127.0.0.1:4747".into())
        .trim_end_matches('/')
        .to_string()
}

pub fn url(path: &str) -> String {
    format!("{}{}", base(), path)
}

/// The server's state, its `--base-dir`: `ZENITH_CODE_HOME` at run time (a development
/// build), else what install.sh built the app with, else `~/.zenith/code`.
fn home() -> PathBuf {
    if let Some(dir) = std::env::var_os("ZENITH_CODE_HOME").filter(|d| !d.is_empty()) {
        return dir.into();
    }
    if let Some(dir) = option_env!("ZENITH_CODE_HOME").filter(|d| !d.is_empty()) {
        return dir.into();
    }
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".zenith").join("code")
}

/// The zenith checkout the app was built from (install.sh builds it in place).
fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// Node: `ZENITH_NODE`, else the one install.sh built the app with, else the usual places
/// (an app started from the Finder has a bare PATH).
fn node() -> PathBuf {
    if let Some(node) = std::env::var_os("ZENITH_NODE").filter(|n| !n.is_empty()) {
        return node.into();
    }
    if let Some(node) = option_env!("ZENITH_NODE").filter(|n| !n.is_empty()) {
        return node.into();
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from))
        .map(|dir| dir.join("node"))
        .find(|node| node.is_file())
        .unwrap_or_else(|| "node".into())
}

/// The server's command line, the one place it is spelled (install.sh has its twin, `SERVER`):
/// the Rust server when install.sh built the app with `ZENITH_SERVER=rust`, else the
/// TypeScript one. Both take the same arguments.
fn server_command() -> Command {
    if option_env!("ZENITH_SERVER") == Some("rust") {
        return Command::new(root().join("target/release/zenith-code"));
    }
    let mut cmd = Command::new(node());
    cmd.arg(root().join("code/apps/server/dist/bin.mjs")).env("NODE_ENV", "production");
    cmd
}

/// The server answers (its environment descriptor, public, on every connection).
pub fn healthy() -> bool {
    let quick: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(2))).build().into();
    quick.get(url("/.well-known/t3/environment")).call().map(|r| r.status() == 200).unwrap_or(false)
}

/// A one-time pairing token (owner scopes, 2 minutes) for the page to sign itself in. The
/// CLI works while the server runs; never log what it returns.
pub fn pairing_token() -> Result<String, String> {
    let output = server_command()
        .args([
            "auth",
            "pairing",
            "create",
            "--ttl",
            "2m",
            "--admin",
            "--label",
            "zenith",
            "--json",
            "--base-dir",
        ])
        .arg(home())
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("zenith code CLI: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output");
        return Err(format!("zenith code CLI failed ({}): {reason}", output.status));
    }
    // Logs may come before the JSON document.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json = stdout.find('{').map(|i| &stdout[i..]).ok_or("no JSON in the CLI output")?;
    let value: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("CLI output: {e}"))?;
    value["credential"]
        .as_str()
        .filter(|c| !c.is_empty())
        .map(String::from)
        .ok_or_else(|| "no credential in the CLI output".into())
}

/// The LaunchAgent's label: `ZENITH_BUNDLE_ID` when install.sh ran, else its default.
fn agent_label() -> &'static str {
    option_env!("ZENITH_AGENT_LABEL").unwrap_or("dev.zenith.app")
}

/// Asks launchd to start the server now, in case it sleeps.
pub fn kickstart() {
    let uid = unsafe { libc_getuid() };
    let _ = Command::new("/bin/launchctl")
        .args(["kickstart", &format!("gui/{uid}/{}", agent_label())])
        .spawn();
}

extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

/// French when install.sh found the Mac speaking French, else when it does now.
pub fn french() -> bool {
    if let Some(lang) = option_env!("ZENITH_LANG") {
        return lang.starts_with("fr");
    }
    Command::new("/usr/bin/defaults")
        .args(["read", "-g", "AppleLanguages"])
        .output()
        .map(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            s.split(['"', ',', '(', '\n', ' ']).find(|w| !w.is_empty()).is_some_and(|w| w.starts_with("fr"))
        })
        .unwrap_or(false)
}
