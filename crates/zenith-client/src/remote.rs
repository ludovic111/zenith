//! A zenith server on another machine, used instead of the local one.
//!
//! One machine can run every agent (a Linux box on the tailnet, published with
//! `zenith-cli setup --tailscale-serve`) while the others only show and drive it. On those,
//! `zenith-cli remote <url> <code>` exchanges a one-time pairing code (`zenith-code auth
//! pairing create --admin` on the server) for a bearer session, and from then on the window,
//! zenith-cli and zenith-mcp talk to that server. `zenith-cli remote --off` goes back to the
//! local server.
//!
//! The server's address is kept in `~/.zenith/app/remote.json`, its session in
//! `~/.zenith/app/remote.token` (0600). `ZENITH_REMOTE_URL` overrides the file for one run
//! (`local` forces the local server). Never log the token.

use std::path::PathBuf;
use std::sync::Arc;

use futures::future::BoxFuture;

use crate::local;
use crate::rpc::Tokens;

/// The file that names the remote server.
pub fn config_path() -> PathBuf {
    local::app_home().join("remote.json")
}

fn token_path() -> PathBuf {
    local::app_home().join("remote.token")
}

/// The remote server's address, when this machine uses one.
pub fn url() -> Option<String> {
    if let Some(value) = std::env::var_os("ZENITH_REMOTE_URL") {
        return from_override(&value.to_string_lossy());
    }
    from_config(&std::fs::read_to_string(config_path()).ok()?)
}

fn from_override(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("local") {
        return None;
    }
    normalize(value).ok()
}

fn from_config(text: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    normalize(value.get("url")?.as_str()?).ok()
}

/// `https://host[:port]`, without a trailing slash or path. A bare host gets `https://`.
pub fn normalize(url: &str) -> anyhow::Result<String> {
    let url = url.trim().trim_end_matches('/');
    let url = if url.contains("://") { url.to_owned() } else { format!("https://{url}") };
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| anyhow::anyhow!("{url}: the address must start with https:// or http://"))?;
    if rest.is_empty() || rest.contains('/') || rest.contains('?') || rest.contains('#') {
        anyhow::bail!("{url}: give the server's address only (https://host or https://host:port)");
    }
    Ok(url)
}

/// The saved session for the remote server.
pub fn saved_token() -> Option<String> {
    std::fs::read_to_string(token_path())
        .ok()
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
}

/// Exchanges a one-time pairing `code` for a session on the server at `url` (RFC 8693 token
/// exchange, `POST /oauth/token`), then makes it this machine's server.
pub async fn pair(url: &str, code: &str, label: &str) -> anyhow::Result<String> {
    let url = normalize(url)?;
    let code = code.trim();
    anyhow::ensure!(!code.is_empty(), "no pairing code");
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?
        .post(format!("{url}/oauth/token"))
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:token-exchange"),
            ("subject_token_type", "urn:t3:params:oauth:token-type:environment-bootstrap"),
            ("requested_token_type", "urn:ietf:params:oauth:token-type:access_token"),
            ("subject_token", code),
            ("client_label", label),
            ("client_device_type", "desktop"),
            ("client_os", std::env::consts::OS),
        ])
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("cannot reach {url}: {e}"))?;
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap_or_default();
    if !status.is_success() {
        let reason = body["reason"].as_str().or(body["error"].as_str()).unwrap_or("refused");
        anyhow::bail!("{url} refused the pairing code ({status}, {reason}): codes are single-use and expire, ask for a new one");
    }
    let token = body["access_token"]
        .as_str()
        .or(body["accessToken"].as_str())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{url} answered without a session"))?;
    local::write_private(&token_path(), token)?;
    local::write_private(&config_path(), &serde_json::json!({ "url": url }).to_string())?;
    Ok(url)
}

/// Goes back to the local server: forgets the remote one and its session.
pub fn forget() -> std::io::Result<bool> {
    let mut forgot = false;
    for path in [config_path(), token_path()] {
        match std::fs::remove_file(&path) {
            Ok(()) => forgot = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(forgot)
}

/// The remote session. Unlike the local one it cannot be reissued from here: when the server
/// refuses it, the machine has to be paired again.
pub struct RemoteToken;

impl RemoteToken {
    pub fn shared() -> Arc<dyn Tokens> {
        Arc::new(Self)
    }
}

impl Tokens for RemoteToken {
    fn token(&self, refused: bool) -> BoxFuture<'_, anyhow::Result<String>> {
        Box::pin(async move {
            let again = "pair this machine again: on the server `zenith-code auth pairing create --admin`, then `zenith-cli remote <url> <code>`";
            if refused {
                anyhow::bail!("the server refused this machine's session; {again}");
            }
            saved_token().ok_or_else(|| anyhow::anyhow!("no session for the remote server; {again}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_normalized() {
        assert_eq!(normalize("https://box.example-tailnet.ts.net/").unwrap(), "https://box.example-tailnet.ts.net");
        assert_eq!(normalize("box.example-tailnet.ts.net").unwrap(), "https://box.example-tailnet.ts.net");
        assert_eq!(normalize(" http://100.64.0.7:4747 ").unwrap(), "http://100.64.0.7:4747");
        assert!(normalize("https://box.example/pair#token=x").is_err());
        assert!(normalize("ftp://box").is_err());
        assert!(normalize("https://").is_err());
    }

    #[test]
    fn the_config_names_the_server() {
        assert_eq!(from_config(r#"{"url":"https://box.example-tailnet.ts.net"}"#).as_deref(), Some("https://box.example-tailnet.ts.net"));
        assert_eq!(from_config(r#"{"url":""}"#), None);
        assert_eq!(from_config("not json"), None);
    }

    #[test]
    fn the_override_can_force_the_local_server() {
        assert_eq!(from_override("local"), None);
        assert_eq!(from_override(""), None);
        assert_eq!(from_override("box.example-tailnet.ts.net").as_deref(), Some("https://box.example-tailnet.ts.net"));
    }
}
