//! The WebSocket RPC to the server: Effect RPC envelopes over text frames (plan §1.3).
//!
//! ```text
//! client → server  {"_tag":"Request","id":N,"tag":"…","payload":…,"headers":[]}
//!                  {"_tag":"Ack","requestId":N}  {"_tag":"Interrupt","requestId":N}  {"_tag":"Ping"}
//! server → client  {"_tag":"Chunk","requestId":N,"values":[…]}  {"_tag":"Exit","requestId":N,"exit":…}
//!                  {"_tag":"Defect","defect":…}  {"_tag":"Pong"}
//! ```
//!
//! One task owns the socket. A supervisor reconnects with a short backoff (0.5 s up to 8 s)
//! and reports [`ConnectionStatus`]; a refused session (401) asks [`Tokens`] for a new one.
//! Calls made while disconnected fail at once with [`RpcError::NotConnected`], and every
//! pending call or stream ends with [`RpcError::Disconnected`] when the socket drops:
//! callers that keep streams open resubscribe when the status comes back to `Connected`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{header, HeaderValue};
use tokio_tungstenite::tungstenite::{self, Message};

use crate::ClientIdentity;

/// Where bearer tokens come from. `refused` is true when the server just refused the last one.
pub trait Tokens: Send + Sync {
    fn token(&self, refused: bool) -> BoxFuture<'_, anyhow::Result<String>>;
}

/// A fixed token (tests, or a token given on the command line).
pub struct FixedToken(pub String);

impl Tokens for FixedToken {
    fn token(&self, _refused: bool) -> BoxFuture<'_, anyhow::Result<String>> {
        Box::pin(async move { Ok(self.0.clone()) })
    }
}

pub struct RpcConfig {
    pub base_url: String,
    pub identity: ClientIdentity,
    pub tokens: Arc<dyn Tokens>,
}

/// The state of the socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionStatus {
    /// Opening the socket (or getting a session first).
    Connecting,
    Connected,
    /// The last attempt failed, or the socket dropped; another attempt comes soon.
    Failed(String),
}

/// Why a call failed.
#[derive(Clone, Debug, thiserror::Error)]
pub enum RpcError {
    /// The method's own typed error (`Exit` cause `Fail`): a tagged error object.
    #[error("{}", describe_failure(.0))]
    Failure(Value),
    /// The server died on the request (`Die`), or sent a `Defect`.
    #[error("zenith server error: {0}")]
    Defect(String),
    #[error("interrupted")]
    Interrupted,
    #[error("the connection to zenith dropped")]
    Disconnected,
    #[error("not connected to zenith ({0})")]
    NotConnected(String),
    #[error("could not encode the request: {0}")]
    Encode(String),
    #[error("unexpected answer: {0}")]
    Decode(String),
}

impl RpcError {
    /// The `_tag` of a typed failure.
    pub fn tag(&self) -> Option<&str> {
        match self {
            Self::Failure(value) => value.get("_tag").and_then(Value::as_str),
            _ => None,
        }
    }
}

/// A tagged error as a sentence: its `message`, else its tag and reason/detail.
pub fn describe_failure(error: &Value) -> String {
    let field = |key: &str| error.get(key).and_then(Value::as_str).filter(|s| !s.is_empty());
    if let Some(message) = field("message").or_else(|| field("detail")) {
        return message.to_owned();
    }
    let tag = field("_tag").unwrap_or("Error");
    match field("reason").or_else(|| field("code")) {
        Some(reason) => format!("{tag}: {reason}"),
        None => tag.to_owned(),
    }
}

/// One stream message.
#[derive(Debug)]
pub enum StreamEvent {
    Item(Value),
    /// The stream is over: `Ok` when the server completed it.
    End(Result<(), RpcError>),
}

enum Reply {
    Unary(oneshot::Sender<Result<Value, RpcError>>),
    Stream(mpsc::UnboundedSender<StreamEvent>),
}

impl Reply {
    fn fail(self, error: RpcError) {
        match self {
            Self::Unary(tx) => {
                let _ = tx.send(Err(error));
            }
            Self::Stream(tx) => {
                let _ = tx.send(StreamEvent::End(Err(error)));
            }
        }
    }
}

enum Command {
    Request { id: u64, tag: String, payload: Value, reply: Reply },
    Interrupt { id: u64 },
}

/// An open stream. Dropping it before it ends interrupts it on the server.
pub struct RpcStream {
    id: u64,
    rx: mpsc::UnboundedReceiver<StreamEvent>,
    commands: mpsc::UnboundedSender<Command>,
    ended: bool,
}

impl RpcStream {
    /// The next item, the end, or `None` once the end was returned.
    pub async fn next(&mut self) -> Option<StreamEvent> {
        if self.ended {
            return None;
        }
        let event = self.rx.recv().await.unwrap_or(StreamEvent::End(Err(RpcError::Disconnected)));
        if matches!(event, StreamEvent::End(_)) {
            self.ended = true;
        }
        Some(event)
    }
}

impl Drop for RpcStream {
    fn drop(&mut self) {
        if !self.ended {
            let _ = self.commands.send(Command::Interrupt { id: self.id });
        }
    }
}

#[derive(Clone)]
pub struct RpcClient {
    commands: mpsc::UnboundedSender<Command>,
    status: watch::Receiver<ConnectionStatus>,
    next_id: Arc<AtomicU64>,
}

impl RpcClient {
    /// Starts the supervisor on the current tokio runtime.
    pub fn spawn(config: RpcConfig) -> Self {
        let (commands, rx) = mpsc::unbounded_channel();
        let (status_tx, status) = watch::channel(ConnectionStatus::Connecting);
        tokio::spawn(supervise(config, rx, status_tx));
        Self {
            commands,
            status,
            next_id: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn status(&self) -> watch::Receiver<ConnectionStatus> {
        self.status.clone()
    }

    pub async fn call(&self, tag: &str, payload: Value) -> Result<Value, RpcError> {
        let (tx, rx) = oneshot::channel();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.commands
            .send(Command::Request {
                id,
                tag: tag.to_owned(),
                payload,
                reply: Reply::Unary(tx),
            })
            .map_err(|_| RpcError::Disconnected)?;
        rx.await.unwrap_or(Err(RpcError::Disconnected))
    }

    pub fn stream(&self, tag: &str, payload: Value) -> RpcStream {
        let (tx, rx) = mpsc::unbounded_channel();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let sent = self.commands.send(Command::Request {
            id,
            tag: tag.to_owned(),
            payload,
            reply: Reply::Stream(tx),
        });
        if let Err(mpsc::error::SendError(Command::Request { reply, .. })) = sent {
            reply.fail(RpcError::Disconnected);
        }
        RpcStream {
            id,
            rx,
            commands: self.commands.clone(),
            ended: false,
        }
    }
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

enum ConnectError {
    Unauthorized,
    Other(String),
}

/// `http://host:port` → `ws://host:port/ws?clientSurface=…&clientAppVersion=…`.
fn socket_url(base_url: &str, identity: &ClientIdentity) -> String {
    let base = base_url.trim_end_matches('/');
    let ws = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_owned()
    };
    let version: String = identity
        .app_version
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
        .collect();
    format!("{ws}/ws?clientSurface={}&clientAppVersion={version}&orchestrationProtocol=1", identity.surface)
}

async fn open(config: &RpcConfig, token: &str) -> Result<Socket, ConnectError> {
    let mut request = socket_url(&config.base_url, &config.identity)
        .into_client_request()
        .map_err(|e| ConnectError::Other(e.to_string()))?;
    let bearer = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| ConnectError::Other("bad token".into()))?;
    request.headers_mut().insert(header::AUTHORIZATION, bearer);
    let connect = tokio_tungstenite::connect_async(request);
    match tokio::time::timeout(Duration::from_secs(20), connect).await {
        Err(_) => Err(ConnectError::Other("timed out".into())),
        Ok(Ok((socket, _))) => Ok(socket),
        Ok(Err(tungstenite::Error::Http(response))) if response.status().as_u16() == 401 => Err(ConnectError::Unauthorized),
        Ok(Err(tungstenite::Error::Http(response))) => Err(ConnectError::Other(format!("HTTP {}", response.status()))),
        Ok(Err(tungstenite::Error::Io(e))) if e.kind() == std::io::ErrorKind::ConnectionRefused => Err(ConnectError::Other("the server is not running".into())),
        Ok(Err(e)) => Err(ConnectError::Other(e.to_string())),
    }
}

const BACKOFF_MS: [u64; 5] = [500, 1000, 2000, 4000, 8000];

async fn supervise(config: RpcConfig, mut commands: mpsc::UnboundedReceiver<Command>, status: watch::Sender<ConnectionStatus>) {
    let mut refused = false;
    let mut attempt = 0usize;
    loop {
        status.send_replace(ConnectionStatus::Connecting);
        let outcome = match config.tokens.token(refused).await {
            Err(error) => Err(ConnectError::Other(format!("no session: {error:#}"))),
            Ok(token) => open(&config, &token).await,
        };
        let reason = match outcome {
            Ok(socket) => {
                refused = false;
                attempt = 0;
                status.send_replace(ConnectionStatus::Connected);
                match run(socket, &mut commands).await {
                    RunEnd::ClientGone => return,
                    RunEnd::Lost(reason) => reason,
                }
            }
            Err(ConnectError::Unauthorized) => {
                // A second refusal in a row is not fixed by another token at once.
                let reason = if refused {
                    "the server refused the session again"
                } else {
                    "the server refused the session"
                };
                refused = true;
                reason.to_owned()
            }
            Err(ConnectError::Other(reason)) => reason,
        };
        tracing::debug!(%reason, "zenith connection lost");
        status.send_replace(ConnectionStatus::Failed(reason.clone()));
        let delay = Duration::from_millis(BACKOFF_MS[attempt.min(BACKOFF_MS.len() - 1)]);
        attempt += 1;
        let sleep = tokio::time::sleep(delay);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                () = &mut sleep => break,
                command = commands.recv() => match command {
                    None => return,
                    Some(Command::Request { reply, .. }) => reply.fail(RpcError::NotConnected(reason.clone())),
                    Some(Command::Interrupt { .. }) => {}
                },
            }
        }
    }
}

enum RunEnd {
    /// Every client handle was dropped.
    ClientGone,
    Lost(String),
}

async fn run(socket: Socket, commands: &mut mpsc::UnboundedReceiver<Command>) -> RunEnd {
    let (mut sink, mut source) = socket.split();
    let mut pending: HashMap<u64, Reply> = HashMap::new();
    let mut ping = tokio::time::interval(Duration::from_secs(5));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await;
    let mut unanswered_pings = 0u32;

    let end = loop {
        tokio::select! {
            command = commands.recv() => match command {
                None => {
                    let _ = sink.close().await;
                    break RunEnd::ClientGone;
                }
                Some(Command::Request { id, tag, payload, reply }) => {
                    let frame = json!({"_tag": "Request", "id": id, "tag": tag, "payload": payload, "headers": []});
                    if let Err(error) = sink.send(Message::text(frame.to_string())).await {
                        reply.fail(RpcError::Disconnected);
                        break RunEnd::Lost(error.to_string());
                    }
                    pending.insert(id, reply);
                }
                Some(Command::Interrupt { id }) => {
                    if pending.remove(&id).is_some() {
                        let frame = json!({"_tag": "Interrupt", "requestId": id});
                        if let Err(error) = sink.send(Message::text(frame.to_string())).await {
                            break RunEnd::Lost(error.to_string());
                        }
                    }
                }
            },
            message = source.next() => {
                unanswered_pings = 0;
                match message {
                    None => break RunEnd::Lost("the server closed the connection".into()),
                    Some(Err(error)) => break RunEnd::Lost(error.to_string()),
                    Some(Ok(Message::Close(_))) => break RunEnd::Lost("the server closed the connection".into()),
                    Some(Ok(Message::Text(text))) => {
                        let replies = handle_frame(text.as_str(), &mut pending);
                        let mut failed = None;
                        for frame in replies {
                            if let Err(error) = sink.send(Message::text(frame.to_string())).await {
                                failed = Some(error.to_string());
                                break;
                            }
                        }
                        if let Some(reason) = failed {
                            break RunEnd::Lost(reason);
                        }
                    }
                    Some(Ok(_)) => {}
                }
            },
            _ = ping.tick() => {
                if unanswered_pings >= 3 {
                    break RunEnd::Lost("the server stopped answering".into());
                }
                unanswered_pings += 1;
                if let Err(error) = sink.send(Message::text(r#"{"_tag":"Ping"}"#)).await {
                    break RunEnd::Lost(error.to_string());
                }
            }
        }
    };
    for (_, reply) in pending.drain() {
        reply.fail(RpcError::Disconnected);
    }
    end
}

/// Applies one server frame to the pending requests; returns the frames to send back
/// (acks, interrupts of streams nobody reads anymore).
fn handle_frame(text: &str, pending: &mut HashMap<u64, Reply>) -> Vec<Value> {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        tracing::warn!("zenith sent a frame that is not JSON");
        return Vec::new();
    };
    let messages = match value {
        Value::Array(items) => items,
        other => vec![other],
    };
    let mut out = Vec::new();
    for message in messages {
        let request_id = message.get("requestId").and_then(Value::as_u64);
        match message.get("_tag").and_then(Value::as_str) {
            Some("Chunk") => {
                let Some(id) = request_id else { continue };
                let Some(Reply::Stream(tx)) = pending.get(&id) else { continue };
                let values = match message.get("values") {
                    Some(Value::Array(values)) => values.clone(),
                    _ => Vec::new(),
                };
                let delivered = values.into_iter().all(|v| tx.send(StreamEvent::Item(v)).is_ok());
                if delivered {
                    out.push(json!({"_tag": "Ack", "requestId": id}));
                } else {
                    pending.remove(&id);
                    out.push(json!({"_tag": "Interrupt", "requestId": id}));
                }
            }
            Some("Exit") => {
                let Some(id) = request_id else { continue };
                let Some(reply) = pending.remove(&id) else { continue };
                let exit = message.get("exit").cloned().unwrap_or(Value::Null);
                let result = decode_exit(&exit);
                match reply {
                    Reply::Unary(tx) => {
                        let _ = tx.send(result);
                    }
                    Reply::Stream(tx) => {
                        let _ = tx.send(StreamEvent::End(result.map(|_| ())));
                    }
                }
            }
            Some("Defect") => {
                let defect = describe_defect(message.get("defect").unwrap_or(&Value::Null));
                for (_, reply) in pending.drain() {
                    reply.fail(RpcError::Defect(defect.clone()));
                }
            }
            _ => {}
        }
    }
    out
}

fn describe_defect(defect: &Value) -> String {
    match defect {
        Value::String(s) => s.clone(),
        Value::Object(o) => o.get("message").and_then(Value::as_str).map(String::from).unwrap_or_else(|| defect.to_string()),
        other => other.to_string(),
    }
}

/// `{"_tag":"Success","value"}` or `{"_tag":"Failure","cause":[…]}`.
fn decode_exit(exit: &Value) -> Result<Value, RpcError> {
    match exit.get("_tag").and_then(Value::as_str) {
        Some("Success") => Ok(exit.get("value").cloned().unwrap_or(Value::Null)),
        Some("Failure") => {
            let causes = exit.get("cause").and_then(Value::as_array).cloned().unwrap_or_default();
            let mut error = RpcError::Defect("unknown failure".into());
            for cause in &causes {
                match cause.get("_tag").and_then(Value::as_str) {
                    Some("Fail") => return Err(RpcError::Failure(cause.get("error").cloned().unwrap_or(Value::Null))),
                    Some("Die") => error = RpcError::Defect(describe_defect(cause.get("defect").unwrap_or(&Value::Null))),
                    Some("Interrupt") if matches!(error, RpcError::Defect(ref d) if d == "unknown failure") => error = RpcError::Interrupted,
                    _ => {}
                }
            }
            Err(error)
        }
        _ => Err(RpcError::Decode(format!("bad exit {exit}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> ClientIdentity {
        ClientIdentity {
            surface: "desktop",
            app_version: "1.2.3 beta".into(),
            session_label: "test",
        }
    }

    #[test]
    fn socket_urls() {
        assert_eq!(
            socket_url("http://127.0.0.1:4747/", &identity()),
            "ws://127.0.0.1:4747/ws?clientSurface=desktop&clientAppVersion=1.2.3beta&orchestrationProtocol=1"
        );
    }

    #[test]
    fn exits() {
        assert_eq!(decode_exit(&json!({"_tag":"Success","value":{"sequence":3}})).unwrap(), json!({"sequence":3}));
        let failure = decode_exit(
            &json!({"_tag":"Failure","cause":[{"_tag":"Fail","error":{"_tag":"EnvironmentAuthorizationError","message":"no scope","requiredScope":"orchestration:read"}}]}),
        );
        let error = failure.unwrap_err();
        assert_eq!(error.tag(), Some("EnvironmentAuthorizationError"));
        assert_eq!(error.to_string(), "no scope");
        assert!(matches!(
            decode_exit(&json!({"_tag":"Failure","cause":[{"_tag":"Die","defect":"Unknown request tag: foo"}]})),
            Err(RpcError::Defect(d)) if d == "Unknown request tag: foo"
        ));
        assert!(matches!(
            decode_exit(&json!({"_tag":"Failure","cause":[{"_tag":"Interrupt","fiberId":null}]})),
            Err(RpcError::Interrupted)
        ));
    }

    #[test]
    fn chunks_are_acked_and_exits_resolve() {
        let mut pending = HashMap::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        pending.insert(1, Reply::Stream(tx));
        let (unary_tx, mut unary_rx) = oneshot::channel();
        pending.insert(2, Reply::Unary(unary_tx));

        let out = handle_frame(
            r#"{"_tag":"Chunk","requestId":1,"values":[{"kind":"synchronized"},{"kind":"x"}]}"#,
            &mut pending,
        );
        assert_eq!(out, vec![json!({"_tag":"Ack","requestId":1})]);
        assert!(matches!(rx.try_recv(), Ok(StreamEvent::Item(v)) if v == json!({"kind":"synchronized"})));
        assert!(matches!(rx.try_recv(), Ok(StreamEvent::Item(_))));

        let out = handle_frame(
            r#"[{"_tag":"Exit","requestId":2,"exit":{"_tag":"Success","value":{}}},{"_tag":"Exit","requestId":1,"exit":{"_tag":"Success","value":null}}]"#,
            &mut pending,
        );
        assert!(out.is_empty());
        assert_eq!(unary_rx.try_recv().unwrap().unwrap(), json!({}));
        assert!(matches!(rx.try_recv(), Ok(StreamEvent::End(Ok(())))));
        assert!(pending.is_empty());
    }

    #[test]
    fn unread_streams_are_interrupted() {
        let mut pending = HashMap::new();
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        pending.insert(7, Reply::Stream(tx));
        let out = handle_frame(r#"{"_tag":"Chunk","requestId":7,"values":[1]}"#, &mut pending);
        assert_eq!(out, vec![json!({"_tag":"Interrupt","requestId":7})]);
        assert!(pending.is_empty());
    }

    #[test]
    fn failures_read_as_sentences() {
        assert_eq!(
            describe_failure(&json!({"_tag":"OrchestrationDispatchCommandError","message":"Thread is busy"})),
            "Thread is busy"
        );
        assert_eq!(
            describe_failure(&json!({"_tag":"EnvironmentAuthInvalidError","reason":"missing_credential"})),
            "EnvironmentAuthInvalidError: missing_credential"
        );
    }
}
