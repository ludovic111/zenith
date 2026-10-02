//! The local zenith server: where it answers, where it keeps its state, waking it up, its
//! command line, and the bearer session local clients sign in with.
//!
//! The server runs on its own (a LaunchAgent on 127.0.0.1:4747, see `scripts/mac/install.sh`),
//! so the window can close and the agents keep working. Every local client (the window,
//! zenith-cli, zenith-mcp) shares one owner session: `zenith-code auth session issue` mints
//! it straight into the server's database, and it is kept in `~/.zenith/app/session.token`
//! (0600). Never log it.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::sync::Mutex;

use crate::rpc::Tokens;

/// `ZENITH_URL` at run time, else what the build was given, else `http://127.0.0.1:4747`.
pub fn base_url() -> String {
    std::env::var("ZENITH_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .or(option_env!("ZENITH_URL").map(String::from))
        .unwrap_or_else(|| "http://127.0.0.1:4747".into())
        .trim_end_matches('/')
        .to_string()
}

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

/// The server's state, its `--base-dir`: `ZENITH_CODE_HOME`, else what the build was
/// given, else `~/.zenith/code`.
pub fn code_home() -> PathBuf {
    if let Some(dir) = std::env::var_os("ZENITH_CODE_HOME").filter(|d| !d.is_empty()) {
        return dir.into();
    }
    if let Some(dir) = option_env!("ZENITH_CODE_HOME").filter(|d| !d.is_empty()) {
        return dir.into();
    }
    home_dir().join(".zenith").join("code")
}

/// The clients' own folder: `ZENITH_APP_HOME`, else `~/.zenith/app` (the session token,
/// the window's preferences, update downloads).
pub fn app_home() -> PathBuf {
    if let Some(dir) = std::env::var_os("ZENITH_APP_HOME").filter(|d| !d.is_empty()) {
        return dir.into();
    }
    home_dir().join(".zenith").join("app")
}

/// The server's log, where the LaunchAgent writes it.
pub fn server_log() -> PathBuf {
    home_dir().join("Library/Logs/Zenith/server.log")
}

/// The `zenith-code` binary: `ZENITH_CODE_BIN`, else next to this program (inside
/// zenith.app, or `target/<profile>/`), else the checkout this was built from, else `PATH`.
pub fn server_binary() -> PathBuf {
    if let Some(bin) = std::env::var_os("ZENITH_CODE_BIN").filter(|b| !b.is_empty()) {
        return bin.into();
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf)) {
        let beside = dir.join("zenith-code");
        if beside.is_file() {
            return beside;
        }
    }
    let checkout = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/release/zenith-code");
    if checkout.is_file() {
        return checkout;
    }
    "zenith-code".into()
}

/// The server answers (its environment descriptor is public).
pub async fn healthy(base_url: &str) -> bool {
    let Ok(client) = reqwest::Client::builder().timeout(Duration::from_secs(2)).build() else {
        return false;
    };
    client
        .get(format!("{base_url}/.well-known/t3/environment"))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// The LaunchAgent's label: `ZENITH_AGENT_LABEL`, else what the build was given, else
/// `dev.zenith.app`.
pub fn agent_label() -> String {
    std::env::var("ZENITH_AGENT_LABEL")
        .ok()
        .filter(|l| !l.is_empty())
        .or(option_env!("ZENITH_AGENT_LABEL").map(String::from))
        .unwrap_or_else(|| "dev.zenith.app".into())
}

/// Starts the server if it sleeps: asks launchd on macOS; elsewhere, starts `zenith-code
/// serve` itself, detached, logging to the state folder.
pub fn kickstart() {
    #[cfg(not(target_os = "macos"))]
    {
        let port = base_url().rsplit(':').next().unwrap_or("4747").trim_end_matches('/').to_owned();
        let log = std::fs::create_dir_all(code_home())
            .ok()
            .and_then(|_| std::fs::OpenOptions::new().create(true).append(true).open(code_home().join("server.log")).ok());
        let mut command = std::process::Command::new(server_binary());
        command
            .args(["serve", "--host", "127.0.0.1", "--port", &port, "--base-dir"])
            .arg(code_home())
            .env("ZENITH_NO_STARTUP_TOKEN", "1")
            .stdin(Stdio::null());
        match log.and_then(|f| f.try_clone().ok().map(|g| (f, g))) {
            Some((out, err)) => {
                command.stdout(out).stderr(err);
            }
            None => {
                command.stdout(Stdio::null()).stderr(Stdio::null());
            }
        }
        let _ = command.spawn();
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { getuid() };
        let _ = std::process::Command::new("/bin/launchctl")
            .args(["kickstart", &format!("gui/{uid}/{}", agent_label())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

/// Restarts the server (after an update replaced its binary).
pub fn restart_server() {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { getuid() };
        let _ = std::process::Command::new("/bin/launchctl")
            .args(["kickstart", "-k", &format!("gui/{uid}/{}", agent_label())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(target_os = "macos")]
extern "C" {
    fn getuid() -> u32;
}

/// Where the shared session token lives.
pub fn token_path() -> PathBuf {
    app_home().join("session.token")
}

/// Reads the saved token, if any.
pub fn saved_token() -> Option<String> {
    std::fs::read_to_string(token_path())
        .ok()
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
}

fn save_token(token: &str) -> std::io::Result<()> {
    let path = token_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    let tmp = path.with_extension("tmp");
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write;
        let mut file = options.open(&tmp)?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(tmp, path)
}

/// Issues a 30-day owner session with the server's command line (it writes straight into
/// the server's database, so it works whether or not the server runs).
pub async fn issue_token(label: &str) -> anyhow::Result<String> {
    let output = tokio::process::Command::new(server_binary())
        .args(["auth", "session", "issue", "--ttl", "30d", "--label", label, "--json", "--base-dir"])
        .arg(code_home())
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("cannot run {}: {e}", server_binary().display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output");
        anyhow::bail!("zenith-code auth session issue failed ({}): {reason}", output.status);
    }
    // Logs may come before the JSON document.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json = stdout
        .find('{')
        .map(|i| &stdout[i..])
        .ok_or_else(|| anyhow::anyhow!("no JSON in the CLI output"))?;
    let value: serde_json::Value = serde_json::from_str(json)?;
    value["token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("no token in the CLI output"))
}

/// The shared session: the saved token, or a new one when there is none or the server
/// refused it.
pub struct TokenSource {
    label: &'static str,
    current: Mutex<Option<String>>,
}

impl TokenSource {
    pub fn new(label: &'static str) -> Self {
        Self {
            label,
            current: Mutex::new(None),
        }
    }
}

impl Tokens for TokenSource {
    fn token(&self, refused: bool) -> BoxFuture<'_, anyhow::Result<String>> {
        Box::pin(async move {
            let mut current = self.current.lock().await;
            if refused {
                // Another client may have renewed it meanwhile.
                let saved = saved_token();
                if saved.is_some() && saved != *current {
                    *current = saved;
                } else {
                    let token = issue_token(self.label).await?;
                    save_token(&token)?;
                    *current = Some(token);
                }
            } else if current.is_none() {
                *current = match saved_token() {
                    Some(token) => Some(token),
                    None => {
                        let token = issue_token(self.label).await?;
                        save_token(&token)?;
                        Some(token)
                    }
                };
            }
            Ok(current.clone().unwrap_or_default())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_file_is_private() {
        let dir = std::env::temp_dir().join(format!("zenith-client-test-{}", crate::new_id()));
        std::env::set_var("ZENITH_APP_HOME", &dir);
        save_token("made-up-token").unwrap();
        assert_eq!(saved_token().as_deref(), Some("made-up-token"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(token_path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(dir).unwrap();
        std::env::remove_var("ZENITH_APP_HOME");
    }
}
