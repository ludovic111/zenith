//! A client of the running zenith server (zenith code, `crates/zenith-code`), shared by the
//! window (`crates/zenith-app`), `zenith-cli` and `zenith-mcp`.
//!
//! - [`local`]: where the server answers and keeps its state, waking it up, and the bearer
//!   session every local client signs in with (issued by `zenith-code auth session issue`,
//!   kept 0600 in `~/.zenith/app/session.token`).
//! - [`rpc`]: the WebSocket RPC (Effect RPC envelopes, plan §1.3 of
//!   `docs/zenith-code-rust-plan.md`): unary calls, streams with acks, ping/pong, and a
//!   supervisor that reconnects and reports its state.
//! - [`Client`]: both together, plus typed helpers over `zc_contracts` for the calls every
//!   client makes (dispatching orchestration commands, reading the shell).
//!
//! - [`remote`]: a server on another machine (paired with `zenith-cli remote`), used instead
//!   of the local one.
//!
//! The client never listens on anything, and only ever talks to the server it was given:
//! the loopback one, or the remote server this machine was paired with.

pub mod local;
pub mod remote;
pub mod rpc;

use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use zc_contracts::RpcMethod;

pub use rpc::{ConnectionStatus, RpcError, RpcStream, StreamEvent};
pub use zc_contracts as contracts;

/// What a client says about itself when it connects (`clientSurface`, `clientAppVersion`):
/// the server stamps it on the events it records.
#[derive(Clone, Debug)]
pub struct ClientIdentity {
    /// `desktop` for the window, `cli` for zenith-cli and zenith-mcp.
    pub surface: &'static str,
    pub app_version: String,
    /// The label of the bearer session, if this client has to issue one.
    pub session_label: &'static str,
}

/// A connection to the server, cheap to clone. It keeps reconnecting until dropped (the
/// last clone).
#[derive(Clone)]
pub struct Client {
    rpc: rpc::RpcClient,
    base_url: Arc<str>,
    tokens: Arc<dyn rpc::Tokens>,
    remote: bool,
}

impl Client {
    /// Connects to the local server ([`local::base_url`]) with the shared bearer session,
    /// issuing one when needed. Must be called inside a tokio runtime.
    pub fn connect_local(identity: ClientIdentity) -> Self {
        let base_url = local::base_url();
        let tokens = local::TokenSource::new(identity.session_label);
        Self::connect(base_url, identity, Arc::new(tokens))
    }

    /// Connects to this machine's server: the remote one when `zenith-cli remote` paired one
    /// ([`remote::url`]), else the local server.
    pub fn connect_default(identity: ClientIdentity) -> Self {
        match remote::url() {
            Some(url) => {
                let mut client = Self::connect(url, identity, remote::RemoteToken::shared());
                client.remote = true;
                client
            }
            None => Self::connect_local(identity),
        }
    }

    /// Connects to `base_url` (`http://127.0.0.1:PORT`, or a remote `https://` server), getting
    /// bearer tokens from `tokens`.
    pub fn connect(base_url: String, identity: ClientIdentity, tokens: Arc<dyn rpc::Tokens>) -> Self {
        let rpc = rpc::RpcClient::spawn(rpc::RpcConfig {
            base_url: base_url.clone(),
            identity,
            tokens: tokens.clone(),
        });
        Self {
            rpc,
            base_url: base_url.into(),
            tokens,
            remote: false,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Whether this is a server on another machine: it cannot be woken up from here, its
    /// folders are not this machine's, and its log is not here.
    pub fn is_remote(&self) -> bool {
        self.remote
    }

    /// A one-time pairing code for a browser, with the owner's scopes (`POST
    /// /api/auth/pairing-token`). Works with any server this client is signed in to.
    pub async fn browser_pairing_code(&self, label: &str) -> anyhow::Result<String> {
        const OWNER: [&str; 8] = [
            "orchestration:read",
            "orchestration:operate",
            "terminal:operate",
            "review:write",
            "relay:read",
            "relay:write",
            "access:read",
            "access:write",
        ];
        let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(20)).build()?;
        let url = format!("{}/api/auth/pairing-token", self.base_url);
        let mut refused = false;
        loop {
            let token = self.tokens.token(refused).await?;
            let response = http
                .post(&url)
                .bearer_auth(token)
                .json(&serde_json::json!({"label": label, "scopes": OWNER}))
                .send()
                .await?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED && !refused {
                refused = true;
                continue;
            }
            let status = response.status();
            anyhow::ensure!(status.is_success(), "/api/auth/pairing-token: HTTP {status}");
            let body: Value = response.json().await?;
            return body["credential"]
                .as_str()
                .filter(|c| !c.is_empty())
                .map(String::from)
                .ok_or_else(|| anyhow::anyhow!("no pairing code in the answer"));
        }
    }

    /// The connection's state now and as it changes.
    pub fn status(&self) -> tokio::sync::watch::Receiver<ConnectionStatus> {
        self.rpc.status()
    }

    /// Waits for the server like a command-line client does: a local server that does not
    /// answer is woken up ([`local::kickstart`]) and given 20 more seconds; a remote one cannot
    /// be woken from here, it only gets more time.
    pub async fn wait_ready(&self) -> Result<(), RpcError> {
        let quick = self.wait_connected(std::time::Duration::from_secs(4)).await;
        if quick.is_ok() {
            return quick;
        }
        if !self.remote {
            local::kickstart();
        }
        self.wait_connected(std::time::Duration::from_secs(20)).await
    }

    /// Waits until the socket is up (or `timeout` passes).
    pub async fn wait_connected(&self, timeout: std::time::Duration) -> Result<(), RpcError> {
        let mut status = self.status();
        let wait = async {
            loop {
                if matches!(*status.borrow_and_update(), ConnectionStatus::Connected) {
                    return Ok(());
                }
                if status.changed().await.is_err() {
                    return Err(RpcError::Disconnected);
                }
            }
        };
        match tokio::time::timeout(timeout, wait).await {
            Ok(result) => result,
            Err(_) => {
                let last = self.status().borrow().clone();
                Err(RpcError::NotConnected(match last {
                    ConnectionStatus::Failed(reason) => reason,
                    other => format!("{other:?}"),
                }))
            }
        }
    }

    /// A unary call with an encoded payload; the encoded success value comes back.
    pub async fn call(&self, tag: &str, payload: Value) -> Result<Value, RpcError> {
        self.rpc.call(tag, payload).await
    }

    /// A typed unary call.
    pub async fn call_typed<M: RpcMethod>(&self, payload: &M::Payload) -> Result<M::Success, RpcError>
    where
        M::Payload: Serialize,
        M::Success: DeserializeOwned,
    {
        let value = serde_json::to_value(payload).map_err(|e| RpcError::Encode(e.to_string()))?;
        let result = self.call(M::TAG, value).await?;
        serde_json::from_value(result).map_err(|e| RpcError::Decode(format!("{}: {e}", M::TAG)))
    }

    /// Opens a stream; dropping the returned handle interrupts it.
    pub fn stream(&self, tag: &str, payload: Value) -> RpcStream {
        self.rpc.stream(tag, payload)
    }

    /// `GET <base><path>` with the session, as JSON (the server's few HTTP-only routes, such
    /// as `/api/zenith/sessions`). Needs a tokio runtime.
    pub async fn get_json(&self, path: &str) -> anyhow::Result<Value> {
        let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(60)).build()?;
        let url = format!("{}{path}", self.base_url);
        let mut refused = false;
        loop {
            let token = self.tokens.token(refused).await?;
            let response = http.get(&url).bearer_auth(token).send().await?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED && !refused {
                refused = true;
                continue;
            }
            let status = response.status();
            if !status.is_success() {
                anyhow::bail!("{path}: HTTP {status}");
            }
            return Ok(response.json().await?);
        }
    }

    /// Sends one orchestration command (`orchestration.dispatchCommand`). The command is the
    /// JSON of a `ClientOrchestrationCommand`; a missing `commandId` or `createdAt` is filled in.
    pub async fn dispatch(&self, mut command: Value) -> Result<i64, RpcError> {
        if let Some(object) = command.as_object_mut() {
            object.entry("commandId").or_insert_with(|| Value::String(new_id()));
            let needs_time = matches!(
                object.get("type").and_then(Value::as_str),
                Some(
                    "thread.create"
                        | "thread.turn.start"
                        | "thread.turn.interrupt"
                        | "thread.approval.respond"
                        | "thread.user-input.respond"
                        | "thread.user-input.dismiss"
                        | "thread.checkpoint.revert"
                        | "thread.conversation.revert"
                        | "thread.session.stop"
                        | "project.create"
                )
            );
            if needs_time {
                object.entry("createdAt").or_insert_with(|| Value::String(now_iso()));
            }
        }
        let result = self.call("orchestration.dispatchCommand", command).await?;
        Ok(result.get("sequence").and_then(Value::as_i64).unwrap_or(0))
    }
}

/// A fresh id for commands, threads, messages (UUID v4, as the web client makes them).
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Now, as the wire writes dates (`2026-10-01T12:00:00.000Z`).
pub fn now_iso() -> String {
    zc_contracts::DateTimeUtc::now().to_iso_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_written_like_the_wire() {
        let now = now_iso();
        assert_eq!(now.len(), 24, "{now}");
        assert!(now.ends_with('Z'));
        assert_eq!(&now[19..20], ".");
    }
}
