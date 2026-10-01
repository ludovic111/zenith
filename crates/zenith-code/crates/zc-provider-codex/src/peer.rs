//! The NDJSON JSON-RPC peer `codex app-server` speaks (`effect-codex-app-server/src/protocol.ts`):
//!
//! - one JSON object per line, no `"jsonrpc"` field on what we write (an incoming one is
//!   ignored), numeric request ids from 1;
//! - requests and notifications both ways. An incoming request runs on its own task (at most
//!   [`MAX_ACTIVE_REQUEST_HANDLERS`] at once, the rest answered `-32001` overloaded), so a
//!   pending approval never blocks the reader; notifications are delivered in wire order on
//!   the reader task;
//! - a line that is not JSON, or a message that is neither a request, a notification nor a
//!   response, ends the protocol (as in TS);
//! - when the input ends the peer terminates once: every pending request fails with the
//!   termination error (the process exit, else "input stream ended"), later sends fail with
//!   it, and running request handlers are aborted.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::AbortHandle;

use crate::errors::{CodexAppServerError, ProtocolParseOperation, RequestError, TransportOperation};

/// `MAX_BUFFERED_RAW_MESSAGES`: the active incoming-request handler limit.
pub const MAX_ACTIVE_REQUEST_HANDLERS: usize = 32;

/// What the peer calls for incoming messages.
pub trait IncomingHandler: Send + Sync + 'static {
    /// Called on the reader task in wire order; keep it quick (hand work to a channel).
    fn on_notification(&self, method: String, params: Option<Value>);
    /// Runs on its own task; the result is the response.
    fn on_request(&self, method: String, params: Option<Value>) -> BoxFuture<'static, Result<Value, RequestError>>;
    /// Called once when the protocol terminates.
    fn on_termination(&self, _error: &CodexAppServerError) {}
}

type PendingSender = oneshot::Sender<Result<Value, CodexAppServerError>>;

struct Inner {
    outgoing: Mutex<Option<mpsc::UnboundedSender<String>>>,
    pending: Mutex<HashMap<String, (String, PendingSender)>>,
    next_request_id: AtomicI64,
    termination: Mutex<Option<CodexAppServerError>>,
    terminated: watch::Sender<bool>,
    handler_tasks: Mutex<HashMap<u64, AbortHandle>>,
    next_handler_id: AtomicI64,
    active_handlers: AtomicUsize,
    handler: Arc<dyn IncomingHandler>,
    background: Mutex<Vec<AbortHandle>>,
}

/// A running peer. Cheap to clone.
#[derive(Clone)]
pub struct CodexPeer {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for CodexPeer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexPeer").finish_non_exhaustive()
    }
}

/// Logs protocol traffic when `ZENITH_CODEX_PROTOCOL_LOG` is set (TS `logIncoming` / `logOutgoing`).
fn log_protocol(direction: &str, line: &str) {
    tracing::trace!(target: "zc_provider_codex::protocol", direction, line);
}

impl CodexPeer {
    /// Starts reading `reader` and writing to `writer`. `termination` resolves to the error the
    /// peer terminates with once the input ends (the process exit status).
    pub fn start<R, W>(reader: R, writer: W, handler: Arc<dyn IncomingHandler>, termination: Option<BoxFuture<'static, CodexAppServerError>>) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<String>();
        let (terminated, _) = watch::channel(false);
        let inner = Arc::new(Inner {
            outgoing: Mutex::new(Some(outgoing_tx)),
            pending: Mutex::new(HashMap::new()),
            next_request_id: AtomicI64::new(1),
            termination: Mutex::new(None),
            terminated,
            handler_tasks: Mutex::new(HashMap::new()),
            next_handler_id: AtomicI64::new(1),
            active_handlers: AtomicUsize::new(0),
            handler,
            background: Mutex::new(Vec::new()),
        });
        let peer = CodexPeer { inner };

        let mut writer = writer;
        let writer_task = tokio::spawn(async move {
            while let Some(line) = outgoing_rx.recv().await {
                log_protocol("outgoing", line.trim_end());
                if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
                    break;
                }
            }
            let _ = writer.shutdown().await;
        });

        let reader_peer = peer.clone();
        let reader_task = tokio::spawn(async move {
            let mut reader = BufReader::new(reader);
            let mut buffer = Vec::new();
            loop {
                buffer.clear();
                match reader.read_until(b'\n', &mut buffer).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let ended = buffer.last() != Some(&b'\n');
                        let mut line = String::from_utf8_lossy(&buffer).into_owned();
                        if line.ends_with('\n') {
                            line.pop();
                        }
                        if line.ends_with('\r') {
                            line.pop();
                        }
                        if let Err(error) = reader_peer.handle_line(&line) {
                            reader_peer.terminate(error);
                            return;
                        }
                        if reader_peer.is_terminated() {
                            return;
                        }
                        if ended {
                            break;
                        }
                    }
                    Err(error) => {
                        reader_peer.terminate(CodexAppServerError::Transport {
                            operation: TransportOperation::ReadInputStream,
                            pid: None,
                            cause: error.to_string(),
                        });
                        return;
                    }
                }
            }
            let error = match termination {
                Some(termination) => termination.await,
                None => CodexAppServerError::InputStreamEnded,
            };
            reader_peer.terminate(error);
        });
        {
            let mut background = peer.inner.background.lock().expect("peer background lock");
            background.push(writer_task.abort_handle());
            background.push(reader_task.abort_handle());
        }
        peer
    }

    /// The error the peer terminated with, if it has.
    pub fn termination(&self) -> Option<CodexAppServerError> {
        self.inner.termination.lock().expect("peer termination lock").clone()
    }

    pub fn is_terminated(&self) -> bool {
        *self.inner.terminated.borrow()
    }

    /// Resolves once the peer has terminated.
    pub async fn wait_terminated(&self) -> CodexAppServerError {
        let mut receiver = self.inner.terminated.subscribe();
        loop {
            if *receiver.borrow_and_update() {
                return self.termination().unwrap_or(CodexAppServerError::InputStreamEnded);
            }
            if receiver.changed().await.is_err() {
                return self.termination().unwrap_or(CodexAppServerError::InputStreamEnded);
            }
        }
    }

    /// Sends a request and waits for its response.
    pub async fn request(&self, method: &str, params: Option<Value>) -> Result<Value, CodexAppServerError> {
        let id = self.inner.next_request_id.fetch_add(1, Ordering::SeqCst);
        let key = id.to_string();
        let (sender, receiver) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .expect("peer pending lock")
            .insert(key.clone(), (method.to_owned(), sender));
        let mut message = Map::new();
        message.insert("id".into(), Value::from(id));
        message.insert("method".into(), Value::from(method));
        if let Some(params) = params {
            message.insert("params".into(), params);
        }
        if let Err(error) = self.offer(Value::Object(message)) {
            self.inner.pending.lock().expect("peer pending lock").remove(&key);
            return Err(error);
        }
        // Dropping this future (a timeout, a cancelled caller) forgets the request, like the
        // TS `onInterrupt(removePending)`.
        let mut guard = PendingGuard { peer: self, key, armed: true };
        let result = match receiver.await {
            Ok(result) => result,
            Err(_) => Err(self.termination().unwrap_or(CodexAppServerError::InputStreamEnded)),
        };
        guard.armed = false;
        result
    }

    /// Sends a notification.
    pub fn notify(&self, method: &str, params: Option<Value>) -> Result<(), CodexAppServerError> {
        let mut message = Map::new();
        message.insert("method".into(), Value::from(method));
        if let Some(params) = params {
            message.insert("params".into(), params);
        }
        self.offer(Value::Object(message))
    }

    /// Answers an incoming request.
    pub fn respond(&self, id: &Value, result: Value) -> Result<(), CodexAppServerError> {
        let mut message = Map::new();
        message.insert("id".into(), id.clone());
        message.insert("result".into(), result);
        self.offer(Value::Object(message))
    }

    /// Answers an incoming request with an error.
    pub fn respond_error(&self, id: &Value, error: &RequestError) -> Result<(), CodexAppServerError> {
        let mut message = Map::new();
        message.insert("id".into(), id.clone());
        message.insert("error".into(), error.to_protocol_error());
        self.offer(Value::Object(message))
    }

    /// Stops the peer: aborts its tasks and request handlers, fails pending requests.
    pub fn shutdown(&self) {
        self.terminate(CodexAppServerError::InputStreamEnded);
        for handle in self.inner.background.lock().expect("peer background lock").drain(..) {
            handle.abort();
        }
    }

    fn offer(&self, message: Value) -> Result<(), CodexAppServerError> {
        if let Some(error) = self.termination() {
            return Err(error);
        }
        let line = format!("{message}\n");
        let outgoing = self.inner.outgoing.lock().expect("peer outgoing lock");
        match outgoing.as_ref() {
            Some(sender) if sender.send(line).is_ok() => Ok(()),
            _ => Err(self.termination().unwrap_or(CodexAppServerError::InputStreamEnded)),
        }
    }

    fn terminate(&self, error: CodexAppServerError) {
        {
            let mut termination = self.inner.termination.lock().expect("peer termination lock");
            if termination.is_some() {
                return;
            }
            *termination = Some(error.clone());
        }
        let pending: Vec<_> = self.inner.pending.lock().expect("peer pending lock").drain().collect();
        for (_, (_, sender)) in pending {
            let _ = sender.send(Err(error.clone()));
        }
        // Ending the queue lets the writer flush what was already accepted, then stop.
        self.inner.outgoing.lock().expect("peer outgoing lock").take();
        for (_, handle) in self.inner.handler_tasks.lock().expect("peer handlers lock").drain() {
            handle.abort();
        }
        self.inner.terminated.send_replace(true);
        self.inner.handler.on_termination(&error);
    }

    fn handle_line(&self, line: &str) -> Result<(), CodexAppServerError> {
        if line.trim().is_empty() {
            return Ok(());
        }
        log_protocol("incoming", line);
        let message: Value = serde_json::from_str(line).map_err(|error| CodexAppServerError::ProtocolParse {
            operation: ProtocolParseOperation::DecodeWireMessage,
            method: None,
            request_id: None,
            detail: Some(format!("line {} column {}", error.line(), error.column())),
        })?;
        self.route(message)
    }

    fn route(&self, message: Value) -> Result<(), CodexAppServerError> {
        let Value::Object(object) = message else {
            return Err(unroutable(&message));
        };
        let method = object.get("method").and_then(Value::as_str).map(str::to_owned);
        let id = object.get("id").filter(|id| id.is_string() || id.is_number()).cloned();
        if let (Some(method), Some(id)) = (&method, &id) {
            self.handle_request(id.clone(), method.clone(), object.get("params").cloned());
            return Ok(());
        }
        if let Some(method) = &method {
            if !object.contains_key("id") {
                self.inner.handler.on_notification(method.clone(), object.get("params").cloned());
                return Ok(());
            }
        }
        if let Some(id) = &id {
            if let Some(response) = parse_response(&object) {
                self.handle_response(id, response);
                return Ok(());
            }
        }
        Err(unroutable(&Value::Object(object)))
    }

    fn handle_response(&self, id: &Value, response: Result<Value, ProtocolErrorParts>) {
        let key = match id {
            Value::String(value) => value.clone(),
            other => other.to_string(),
        };
        let Some((method, sender)) = self.inner.pending.lock().expect("peer pending lock").remove(&key) else {
            return;
        };
        let _ = sender.send(
            response.map_err(|(code, message, data)| CodexAppServerError::Request(RequestError::from_protocol_error(code, message, data, &method, &key))),
        );
    }

    fn handle_request(&self, id: Value, method: String, params: Option<Value>) {
        let accepted = self
            .inner
            .active_handlers
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                (count < MAX_ACTIVE_REQUEST_HANDLERS).then_some(count + 1)
            })
            .is_ok();
        if !accepted {
            let _ = self.respond_error(&id, &RequestError::overloaded("Too many Codex requests are already active."));
            return;
        }
        let peer = self.clone();
        let future = self.inner.handler.on_request(method.clone(), params);
        #[allow(clippy::cast_sign_loss)]
        let task_id = self.inner.next_handler_id.fetch_add(1, Ordering::SeqCst) as u64;
        // Held while spawning so the task's own removal runs after the insertion below.
        let mut tasks = self.inner.handler_tasks.lock().expect("peer handlers lock");
        let handle = tokio::spawn(async move {
            let result = future.await;
            peer.inner.active_handlers.fetch_sub(1, Ordering::SeqCst);
            peer.inner.handler_tasks.lock().expect("peer handlers lock").remove(&task_id);
            let sent = match result {
                Ok(result) => peer.respond(&id, result),
                Err(error) => peer.respond_error(&id, &error),
            };
            if let Err(error) = sent {
                tracing::debug!(%method, %error, "could not answer a Codex request");
            }
        });
        tasks.insert(task_id, handle.abort_handle());
        drop(tasks);
        if self.is_terminated() {
            handle.abort();
        }
    }
}

struct PendingGuard<'a> {
    peer: &'a CodexPeer,
    key: String,
    armed: bool,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.peer.inner.pending.lock().expect("peer pending lock").remove(&self.key);
        }
    }
}

/// A JSON-RPC error answer: code, message, data.
type ProtocolErrorParts = (i64, String, Option<Value>);

/// `JsonRpcResponseEnvelope`: `{id, result?, error?: {code: number, message: string, data?}}`.
fn parse_response(object: &Map<String, Value>) -> Option<Result<Value, ProtocolErrorParts>> {
    match object.get("error") {
        None => Some(Ok(object.get("result").cloned().unwrap_or(Value::Null))),
        Some(Value::Object(error)) => {
            let code = error.get("code").and_then(Value::as_f64)?;
            let message = error.get("message").and_then(Value::as_str)?;
            #[allow(clippy::cast_possible_truncation)]
            Some(Err((code as i64, message.to_owned(), error.get("data").cloned())))
        }
        Some(_) => None,
    }
}

fn unroutable(message: &Value) -> CodexAppServerError {
    let kind = match message {
        Value::Null => "null",
        Value::Array(_) => "array",
        Value::String(_) => "string",
        Value::Number(_) => "number",
        Value::Bool(_) => "boolean",
        Value::Object(_) => "object",
    };
    let (method, request_id, fields) = match message {
        Value::Object(object) => (
            object.get("method").and_then(Value::as_str).map(str::to_owned),
            object.get("id").and_then(|id| match id {
                Value::String(value) => Some(value.clone()),
                Value::Number(value) => Some(value.to_string()),
                _ => None,
            }),
            ["id", "method", "params", "result", "error"]
                .into_iter()
                .filter(|field| object.contains_key(*field))
                .collect::<Vec<_>>()
                .join(","),
        ),
        _ => (None, None, String::new()),
    };
    CodexAppServerError::ProtocolParse {
        operation: ProtocolParseOperation::RouteWireMessage,
        method,
        request_id,
        detail: Some(format!("payloadKind={kind} presentFields=[{fields}]")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

    #[derive(Default)]
    struct Recorder {
        notifications: Mutex<Vec<(String, Option<Value>)>>,
        requests: Mutex<Vec<(String, Option<Value>)>>,
        terminations: Mutex<Vec<CodexAppServerError>>,
        hold: Mutex<Option<tokio::sync::watch::Receiver<bool>>>,
    }

    impl IncomingHandler for Recorder {
        fn on_notification(&self, method: String, params: Option<Value>) {
            self.notifications.lock().unwrap().push((method, params));
        }
        fn on_request(&self, method: String, params: Option<Value>) -> BoxFuture<'static, Result<Value, RequestError>> {
            self.requests.lock().unwrap().push((method.clone(), params));
            let hold = self.hold.lock().unwrap().clone();
            Box::pin(async move {
                if let Some(mut hold) = hold {
                    while !*hold.borrow_and_update() {
                        if hold.changed().await.is_err() {
                            break;
                        }
                    }
                }
                if method == "item/tool/requestUserInput" {
                    Ok(serde_json::json!({"answers": {"approved": {"answers": ["yes"]}}}))
                } else {
                    Err(RequestError::method_not_found(&method))
                }
            })
        }
        fn on_termination(&self, error: &CodexAppServerError) {
            self.terminations.lock().unwrap().push(error.clone());
        }
    }

    struct Harness {
        peer: CodexPeer,
        recorder: Arc<Recorder>,
        input: DuplexStream,
        output: BufReader<DuplexStream>,
    }

    fn harness(recorder: Recorder) -> Harness {
        let (peer_reader, input) = tokio::io::duplex(1 << 20);
        let (output, peer_writer) = tokio::io::duplex(1 << 20);
        let recorder = Arc::new(recorder);
        let peer = CodexPeer::start(peer_reader, peer_writer, recorder.clone(), None);
        Harness {
            peer,
            recorder,
            input,
            output: BufReader::new(output),
        }
    }

    async fn next_line(output: &mut BufReader<DuplexStream>) -> Value {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), output.read_line(&mut line))
            .await
            .expect("a line in time")
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    #[tokio::test]
    async fn encodes_requests_without_jsonrpc_and_routes_both_directions() {
        let mut h = harness(Recorder::default());
        h.peer.notify("initialized", None).unwrap();
        let mut raw = String::new();
        h.output.read_line(&mut raw).await.unwrap();
        assert_eq!(raw, "{\"method\":\"initialized\"}\n");

        let params = serde_json::json!({"clientInfo": {"name": "test", "title": "Test", "version": "0.0.0"}, "capabilities": {"experimentalApi": true}});
        let peer = h.peer.clone();
        let sent = params.clone();
        let pending = tokio::spawn(async move { peer.request("initialize", Some(sent)).await });
        assert_eq!(
            next_line(&mut h.output).await,
            serde_json::json!({"id": 1, "method": "initialize", "params": params})
        );

        h.input
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"item/agentMessage/delta\",\"params\":{\"delta\":\"Hello\",\"itemId\":\"item-1\",\"threadId\":\"thread-1\",\"turnId\":\"turn-1\"}}\n")
            .await
            .unwrap();
        h.input
            .write_all(
                b"{\"id\":77,\"method\":\"item/tool/requestUserInput\",\"params\":{\"itemId\":\"i\",\"threadId\":\"t\",\"turnId\":\"u\",\"questions\":[]}}\r\n",
            )
            .await
            .unwrap();
        h.input
            .write_all(b"{\"id\":1,\"result\":{\"userAgent\":\"mock\",\"codexHome\":\"/tmp\",\"platformFamily\":\"unix\",\"platformOs\":\"macos\"}}\n")
            .await
            .unwrap();
        let result = pending.await.unwrap().unwrap();
        assert_eq!(result["userAgent"], "mock");
        assert_eq!(
            next_line(&mut h.output).await,
            serde_json::json!({"id": 77, "result": {"answers": {"approved": {"answers": ["yes"]}}}})
        );
        assert_eq!(h.recorder.notifications.lock().unwrap()[0].0, "item/agentMessage/delta");
        assert_eq!(h.recorder.requests.lock().unwrap()[0].0, "item/tool/requestUserInput");

        h.input.write_all(b"{\"id\":\"x-1\",\"method\":\"unknown/method\"}\n").await.unwrap();
        assert_eq!(
            next_line(&mut h.output).await,
            serde_json::json!({"id": "x-1", "error": {"code": -32601, "message": "Method not found: unknown/method"}})
        );
    }

    #[tokio::test]
    async fn routes_a_large_notification_fragmented_across_many_chunks() {
        let mut h = harness(Recorder::default());
        let text = "x".repeat(200_000);
        let line = format!("{}\n", serde_json::json!({"method": "item/agentMessage/delta", "params": {"delta": text}}));
        for chunk in line.as_bytes().chunks(37) {
            h.input.write_all(chunk).await.unwrap();
        }
        settle().await;
        let notifications = h.recorder.notifications.lock().unwrap();
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].1.as_ref().unwrap()["delta"].as_str().unwrap().len(), 200_000);
    }

    #[tokio::test]
    async fn a_malformed_final_line_terminates_before_the_input_ends() {
        let mut h = harness(Recorder::default());
        h.input.write_all(b"{\"method\":\"x\"").await.unwrap();
        h.input.shutdown().await.unwrap();
        let error = tokio::time::timeout(Duration::from_secs(5), h.peer.wait_terminated()).await.unwrap();
        assert!(matches!(
            error,
            CodexAppServerError::ProtocolParse {
                operation: ProtocolParseOperation::DecodeWireMessage,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn the_input_ending_is_its_own_termination_and_fails_pending_requests() {
        let mut h = harness(Recorder::default());
        let peer = h.peer.clone();
        let pending = tokio::spawn(async move { peer.request("thread/start", None).await });
        let _ = next_line(&mut h.output).await;
        h.input.shutdown().await.unwrap();
        let error = pending.await.unwrap().unwrap_err();
        assert_eq!(error, CodexAppServerError::InputStreamEnded);
        assert_eq!(h.recorder.terminations.lock().unwrap().len(), 1);
        assert_eq!(h.peer.notify("x", None).unwrap_err(), CodexAppServerError::InputStreamEnded);
    }

    #[tokio::test]
    async fn keeps_processing_messages_while_an_approval_is_pending() {
        let (release, hold) = tokio::sync::watch::channel(false);
        let recorder = Recorder {
            hold: Mutex::new(Some(hold)),
            ..Recorder::default()
        };
        let mut h = harness(recorder);
        h.input
            .write_all(b"{\"id\":5,\"method\":\"item/tool/requestUserInput\",\"params\":{}}\n{\"method\":\"turn/started\",\"params\":{}}\n")
            .await
            .unwrap();
        settle().await;
        assert_eq!(h.recorder.notifications.lock().unwrap().len(), 1);
        release.send_replace(true);
        assert_eq!(next_line(&mut h.output).await["id"], 5);
    }

    #[tokio::test]
    async fn rejects_incoming_requests_past_the_active_handler_limit() {
        let (_release, hold) = tokio::sync::watch::channel(false);
        let recorder = Recorder {
            hold: Mutex::new(Some(hold)),
            ..Recorder::default()
        };
        let mut h = harness(recorder);
        let mut lines = String::new();
        for id in 0..=MAX_ACTIVE_REQUEST_HANDLERS {
            lines.push_str(&format!("{{\"id\":{id},\"method\":\"item/tool/requestUserInput\",\"params\":{{}}}}\n"));
        }
        h.input.write_all(lines.as_bytes()).await.unwrap();
        let rejected = next_line(&mut h.output).await;
        assert_eq!(rejected["id"], MAX_ACTIVE_REQUEST_HANDLERS);
        assert_eq!(rejected["error"]["code"], -32001);
        assert_eq!(rejected["error"]["message"], "Too many Codex requests are already active.");
    }

    #[tokio::test]
    async fn correlates_response_errors_with_the_originating_request() {
        let mut h = harness(Recorder::default());
        let peer = h.peer.clone();
        let pending = tokio::spawn(async move { peer.request("turn/start", Some(serde_json::json!({}))).await });
        let sent = next_line(&mut h.output).await;
        h.input
            .write_all(
                format!(
                    "{{\"id\":{},\"error\":{{\"code\":-32000,\"message\":\"nope\",\"data\":{{\"x\":1}}}}}}\n",
                    sent["id"]
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let error = pending.await.unwrap().unwrap_err();
        let CodexAppServerError::Request(error) = error else {
            panic!("request error expected")
        };
        assert_eq!(error.code, -32000);
        assert_eq!(error.message, "nope");
        assert_eq!(error.method.as_deref(), Some("turn/start"));
        assert_eq!(error.request_id.as_deref(), Some("1"));
        assert_eq!(error.data, Some(serde_json::json!({"x": 1})));
    }

    #[tokio::test]
    async fn unroutable_messages_terminate_with_structural_diagnostics() {
        let mut h = harness(Recorder::default());
        h.input.write_all(b"{\"id\":null,\"secret\":\"value\"}\n").await.unwrap();
        let error = tokio::time::timeout(Duration::from_secs(5), h.peer.wait_terminated()).await.unwrap();
        let CodexAppServerError::ProtocolParse { operation, detail, .. } = error else {
            panic!("parse error expected")
        };
        assert_eq!(operation, ProtocolParseOperation::RouteWireMessage);
        assert!(!detail.unwrap_or_default().contains("value"));
    }

    #[tokio::test]
    async fn termination_aborts_running_request_handlers() {
        let (_release, hold) = tokio::sync::watch::channel(false);
        let recorder = Recorder {
            hold: Mutex::new(Some(hold)),
            ..Recorder::default()
        };
        let mut h = harness(recorder);
        h.input
            .write_all(b"{\"id\":1,\"method\":\"item/tool/requestUserInput\",\"params\":{}}\n")
            .await
            .unwrap();
        settle().await;
        assert_eq!(h.peer.inner.handler_tasks.lock().unwrap().len(), 1);
        h.input.shutdown().await.unwrap();
        h.peer.wait_terminated().await;
        assert!(h.peer.inner.handler_tasks.lock().unwrap().is_empty());
    }
}
