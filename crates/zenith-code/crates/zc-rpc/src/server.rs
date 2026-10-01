//! The per-connection protocol engine (plan §1.3 rules 1–8).
//!
//! One socket = one reader loop, one writer loop and one task per request. The reader
//! never waits on the socket's write side: `Pong`, protocol `Defect`s and replies that
//! need no handler go through an unbounded control queue that the writer drains first,
//! so pings are answered at once whatever the data traffic. Chunks and `Exit`s go through
//! a bounded data queue; a stream task also waits for the client's `Ack` (or for room
//! in its ack window) before producing its next chunk.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::{FutureExt, Sink, SinkExt, Stream, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, Notify};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::context::{AuthContext, ConnectionInfo, RequestContext};
use crate::error::{authorization_error, panic_message, RpcError};
use crate::exit::Exit;
use crate::message::{decode_frame, encode_chunk, error_defect, ClientMessage, RequestId, ServerMessage};
use crate::router::{AckWindow, ErasedPayload, Handler, Method, RpcRouter, StreamFn};

/// A frame read from the socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inbound {
    Text(String),
    /// Decoded as UTF-8 JSON, like the TS parser does with bytes.
    Binary(Vec<u8>),
    Close,
}

/// A frame to write to the socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outbound {
    Text(String),
    Close { code: u16, reason: String },
}

/// Tuning knobs. The defaults suit the web client.
#[derive(Clone, Debug)]
pub struct ServerOptions {
    /// Data frames (chunks, exits) queued per connection before stream tasks wait.
    pub outbound_buffer: usize,
    /// Most values batched into one chunk from what a stream has ready.
    pub max_batch_values: usize,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            outbound_buffer: 64,
            max_batch_values: 1024,
        }
    }
}

/// What the HTTP layer knows about a new connection.
#[derive(Debug, Default)]
pub struct ConnectionSetup {
    pub auth: AuthContext,
    pub metadata: BTreeMap<String, String>,
    pub extensions: http::Extensions,
}

/// The RPC server: the method table plus every live connection.
pub struct RpcServer {
    router: Arc<RpcRouter>,
    options: ServerOptions,
    shutdown: CancellationToken,
    connections: TaskTracker,
    next_connection: AtomicU64,
    close_listeners: Mutex<Vec<CloseListener>>,
}

/// Called with a connection's id once it has closed and every request of it has ended (the
/// per-socket finalizers of `ws.ts`, e.g. dropping the socket's background-activity leases).
pub type CloseListener = Arc<dyn Fn(u64) + Send + Sync>;

impl RpcServer {
    pub fn new(router: RpcRouter) -> Arc<Self> {
        Self::with_options(router, ServerOptions::default())
    }

    pub fn with_options(router: RpcRouter, options: ServerOptions) -> Arc<Self> {
        Arc::new(Self {
            router: Arc::new(router),
            options,
            shutdown: CancellationToken::new(),
            connections: TaskTracker::new(),
            next_connection: AtomicU64::new(1),
            close_listeners: Mutex::new(Vec::new()),
        })
    }

    /// Runs `listener` after every connection closes (see [`CloseListener`]).
    pub fn on_connection_closed(&self, listener: CloseListener) {
        self.close_listeners.lock().unwrap_or_else(|p| p.into_inner()).push(listener);
    }

    pub fn router(&self) -> &RpcRouter {
        &self.router
    }

    /// Live connections.
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutdown.is_cancelled()
    }

    /// Interrupts every request, closes every socket (code 1001) and waits for all of
    /// it to finish. New sockets are closed at once.
    pub async fn shutdown(&self) {
        self.shutdown.cancel();
        self.connections.close();
        self.connections.wait().await;
    }

    /// Serves one socket until the client closes it, it breaks, or the server shuts down.
    /// Returns once every request task of the connection has ended.
    pub async fn serve_socket<R, W>(self: &Arc<Self>, setup: ConnectionSetup, inbound: R, outbound: W)
    where
        R: Stream<Item = Inbound> + Unpin + Send,
        W: Sink<Outbound> + Unpin + Send,
    {
        let mut outbound = outbound;
        if self.shutdown.is_cancelled() {
            let _ = outbound.send(going_away()).await;
            return;
        }
        let id = self.next_connection.fetch_add(1, Ordering::Relaxed);
        let fut = self.clone().run_connection(id, setup, inbound, outbound);
        self.connections.track_future(fut).await;
    }

    async fn run_connection<R, W>(self: Arc<Self>, id: u64, setup: ConnectionSetup, mut inbound: R, mut outbound: W)
    where
        R: Stream<Item = Inbound> + Unpin + Send,
        W: Sink<Outbound> + Unpin + Send,
    {
        let (control_tx, mut control_rx) = mpsc::unbounded_channel::<String>();
        let (data_tx, mut data_rx) = mpsc::channel::<String>(self.options.outbound_buffer.max(1));
        let cancel = self.shutdown.child_token();
        let conn = Arc::new(Connection {
            info: Arc::new(ConnectionInfo {
                id,
                auth: setup.auth,
                metadata: setup.metadata,
                extensions: setup.extensions,
            }),
            router: self.router.clone(),
            cancel: cancel.clone(),
            inflight: Mutex::new(HashMap::new()),
            control: control_tx,
            data: data_tx,
            tasks: TaskTracker::new(),
            max_batch: self.options.max_batch_values.max(1),
        });
        tracing::debug!(connection = id, "rpc connection opened");

        let reader = {
            let conn = conn.clone();
            async move {
                loop {
                    let frame = tokio::select! {
                        biased;
                        _ = conn.cancel.cancelled() => break,
                        frame = inbound.next() => frame,
                    };
                    match frame {
                        Some(Inbound::Text(text)) => conn.handle_text(&text),
                        Some(Inbound::Binary(bytes)) => match String::from_utf8(bytes) {
                            Ok(text) => conn.handle_text(&text),
                            Err(e) => conn.send_control(&ServerMessage::Defect {
                                defect: error_defect("TypeError", &e.to_string()),
                            }),
                        },
                        Some(Inbound::Close) | None => break,
                    }
                }
                // The client is gone (or the server is stopping): interrupt everything.
                conn.cancel.cancel();
            }
        };

        let writer = {
            let cancel = cancel.clone();
            let shutdown = self.shutdown.clone();
            async move {
                loop {
                    let frame = tokio::select! {
                        biased;
                        Some(frame) = control_rx.recv() => frame,
                        Some(frame) = data_rx.recv() => frame,
                        _ = cancel.cancelled() => break,
                    };
                    if outbound.send(Outbound::Text(frame)).await.is_err() {
                        cancel.cancel();
                        break;
                    }
                }
                if shutdown.is_cancelled() {
                    let _ = outbound.send(going_away()).await;
                }
                let _ = outbound.close().await;
            }
        };

        tokio::join!(reader, writer);
        conn.tasks.close();
        conn.tasks.wait().await;
        tracing::debug!(connection = id, "rpc connection closed");
        let listeners = self.close_listeners.lock().unwrap_or_else(|p| p.into_inner()).clone();
        for listener in listeners {
            listener(id);
        }
    }
}

fn going_away() -> Outbound {
    Outbound::Close {
        code: 1001,
        reason: "server shutting down".into(),
    }
}

struct Inflight {
    cancel: CancellationToken,
    window: Arc<WindowState>,
}

struct Connection {
    info: Arc<ConnectionInfo>,
    router: Arc<RpcRouter>,
    cancel: CancellationToken,
    inflight: Mutex<HashMap<RequestId, Inflight>>,
    control: mpsc::UnboundedSender<String>,
    data: mpsc::Sender<String>,
    tasks: TaskTracker,
    max_batch: usize,
}

impl Connection {
    fn send_control(&self, message: &ServerMessage) {
        let _ = self.control.send(message.encode());
    }

    /// Queues a data frame; false once the connection is going away.
    async fn send_data(&self, frame: String) -> bool {
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => false,
            sent = self.data.send(frame) => sent.is_ok(),
        }
    }

    fn handle_text(self: &Arc<Self>, text: &str) {
        match decode_frame(text) {
            Err(error) => self.send_control(&ServerMessage::Defect { defect: error.defect() }),
            Ok(messages) => {
                for message in messages {
                    match message {
                        Ok(message) => self.handle(message),
                        Err(error) => self.send_control(&ServerMessage::Defect { defect: error.defect() }),
                    }
                }
            }
        }
    }

    fn handle(self: &Arc<Self>, message: ClientMessage) {
        match message {
            ClientMessage::Request { id, tag, payload } => self.start_request(id, tag, payload),
            ClientMessage::Ack { request_id } => {
                let inflight = self.inflight.lock().unwrap();
                if let Some(request) = inflight.get(&request_id) {
                    request.window.ack();
                }
            }
            ClientMessage::Interrupt { request_id } => {
                // An unknown id (already finished) gets no reply: the TS server drops it
                // too, since it no longer has schemas to encode the Exit with.
                let inflight = self.inflight.lock().unwrap();
                if let Some(request) = inflight.get(&request_id) {
                    request.cancel.cancel();
                }
            }
            ClientMessage::Ping => self.send_control(&ServerMessage::Pong),
            // The socket client never sends it; requests in flight carry on.
            ClientMessage::Eof | ClientMessage::Ignored => {}
        }
    }

    fn start_request(self: &Arc<Self>, id: RequestId, tag: String, payload: Value) {
        if self.inflight.lock().unwrap().contains_key(&id) {
            tracing::debug!(connection = self.info.id, %id, "duplicate request id ignored");
            return;
        }
        let Some(method) = self.router.get(&tag).cloned() else {
            let exit = RpcError::die_text(format!("Unknown request tag: {tag}")).into_exit();
            self.send_control(&ServerMessage::Exit { request_id: id, exit });
            return;
        };
        let required = method.scope.scope_for(&payload);
        let decoded = match (method.decode)(payload) {
            Ok(decoded) => decoded,
            Err(issue) => {
                let exit = RpcError::die_text(issue).into_exit();
                self.send_control(&ServerMessage::Exit { request_id: id, exit });
                return;
            }
        };
        if let Some(scope) = required {
            if !self.info.auth.has_scope(&scope) {
                let exit = authorization_error(&scope).into_exit();
                self.send_control(&ServerMessage::Exit { request_id: id, exit });
                return;
            }
        }
        let cancel = self.cancel.child_token();
        let window = Arc::new(WindowState::new(method.ack_window));
        self.inflight.lock().unwrap().insert(
            id.clone(),
            Inflight {
                cancel: cancel.clone(),
                window: window.clone(),
            },
        );
        let ctx = RequestContext {
            connection: self.info.clone(),
            request_id: id.clone(),
            tag: method.tag.clone(),
            cancel: cancel.clone(),
        };
        let conn = self.clone();
        let span = rpc_span(&method.tag);
        self.tasks.spawn(async move {
            let exit = tracing::Instrument::instrument(conn.run(&method, ctx, decoded, &cancel, &window), span.clone()).await;
            record_exit(&span, &exit);
            conn.inflight.lock().unwrap().remove(&id);
            if !conn.cancel.is_cancelled() {
                let frame = ServerMessage::Exit { request_id: id, exit }.encode();
                conn.send_data(frame).await;
            }
        });
    }

    async fn run(&self, method: &Method, ctx: RequestContext, payload: ErasedPayload, cancel: &CancellationToken, window: &WindowState) -> Exit {
        match &method.handler {
            Handler::Unary(handler) => {
                let call = AssertUnwindSafe(handler(ctx, payload)).catch_unwind();
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Exit::interrupt(),
                    result = call => match result {
                        Ok(Ok(value)) => Exit::success(value),
                        Ok(Err(error)) => error.into_exit(),
                        Err(panic) => panicked(&*panic),
                    },
                }
            }
            Handler::Stream(handler) => self.run_stream(handler, ctx, payload, cancel, window).await,
        }
    }

    async fn run_stream(&self, handler: &StreamFn, ctx: RequestContext, payload: ErasedPayload, cancel: &CancellationToken, window: &WindowState) -> Exit {
        let request_id = ctx.request_id.clone();
        let setup = AssertUnwindSafe(handler(ctx, payload)).catch_unwind();
        let stream = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Exit::interrupt(),
            result = setup => match result {
                Ok(Ok(stream)) => stream,
                Ok(Err(error)) => return error.into_exit(),
                Err(panic) => return panicked(&*panic),
            },
        };
        let mut stream = AssertUnwindSafe(stream).catch_unwind();
        loop {
            let first = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Exit::interrupt(),
                item = stream.next() => item,
            };
            let mut values = Vec::new();
            match first {
                None => return Exit::stream_end(),
                Some(Err(panic)) => return panicked(&*panic),
                Some(Ok(Err(error))) => return error.into_exit(),
                Some(Ok(Ok(value))) => values.push(value),
            }
            // Batch whatever else is ready right now; remember how the stream ended if it
            // did, to answer after this last chunk is acknowledged (as Effect does).
            let mut end = None;
            while values.len() < self.max_batch {
                match stream.next().now_or_never() {
                    None => break,
                    Some(None) => {
                        end = Some(Exit::stream_end());
                        break;
                    }
                    Some(Some(Err(panic))) => {
                        end = Some(panicked(&*panic));
                        break;
                    }
                    Some(Some(Ok(Err(error)))) => {
                        end = Some(error.into_exit());
                        break;
                    }
                    Some(Some(Ok(Ok(value)))) => values.push(value),
                }
            }
            let frame = encode_chunk(&request_id, values);
            window.sent(frame.len());
            if !self.send_data(frame).await {
                return Exit::interrupt();
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Exit::interrupt(),
                _ = window.ready() => {}
            }
            if let Some(exit) = end {
                return exit;
            }
        }
    }
}

/// Methods whose calls are not traced (`RPC_METHODS_WITH_TRACING_DISABLED` of
/// `RpcInstrumentation.ts`): reading diagnostics must not add to them.
const RPC_METHODS_WITH_TRACING_DISABLED: &[&str] = &[
    "server.getTraceDiagnostics",
    "server.getProcessDiagnostics",
    "server.getProcessResourceHistory",
    "server.signalProcess",
];

/// The `ws.rpc.<method>` span of one request (target `zenith::trace`: the trace file records
/// it, the console log does not show it). Disabled spans cost nothing.
fn rpc_span(tag: &str) -> tracing::Span {
    if RPC_METHODS_WITH_TRACING_DISABLED.contains(&tag) {
        return tracing::Span::none();
    }
    tracing::info_span!(
        target: "zenith::trace",
        "ws.rpc",
        otel.name = %format!("ws.rpc.{tag}"),
        rpc.method = %tag,
        rpc.transport = "websocket",
        rpc.system = "effect-rpc",
        exit.tag = tracing::field::Empty,
        exit.cause = tracing::field::Empty,
    )
}

/// `exit.tag` and `exit.cause` of a finished request (`Cause.pretty`-like text).
fn record_exit(span: &tracing::Span, exit: &Exit) {
    use crate::exit::CauseReason;
    let Exit::Failure(reasons) = exit else {
        return;
    };
    if span.is_disabled() {
        return;
    }
    let describe = |value: &Value| -> String {
        let name = value.get("_tag").or_else(|| value.get("name")).and_then(Value::as_str);
        let message = value.get("message").and_then(Value::as_str);
        match (name, message) {
            (Some(name), Some(message)) => format!("{name}: {message}"),
            (Some(name), None) => name.to_owned(),
            (None, Some(message)) => message.to_owned(),
            (None, None) => value.to_string(),
        }
    };
    let failures: Vec<String> = reasons
        .iter()
        .filter_map(|reason| match reason {
            CauseReason::Fail(error) => Some(describe(error)),
            CauseReason::Die(defect) => Some(describe(defect)),
            CauseReason::Interrupt(_) => None,
        })
        .collect();
    if failures.is_empty() {
        span.record("exit.tag", "Interrupted");
        span.record("exit.cause", "All fibers interrupted without error");
    } else {
        span.record("exit.tag", "Failure");
        span.record("exit.cause", failures.join("\n").as_str());
    }
}

fn panicked(payload: &(dyn std::any::Any + Send)) -> Exit {
    let message = panic_message(payload);
    tracing::warn!("rpc handler panicked: {message}");
    Exit::die(error_defect("Error", &message))
}

/// Chunks sent and not yet acknowledged. With [`AckWindow::PER_CHUNK`] the window is
/// full after every chunk, so the stream waits for each `Ack`; with a larger window the
/// stream runs ahead until it fills (the server "acknowledges for the client" meanwhile).
/// `Ack`s with nothing outstanding are ignored, like Effect's latch.
pub(crate) struct WindowState {
    limits: AckWindow,
    pending: Mutex<(VecDeque<usize>, usize)>,
    notify: Notify,
}

impl WindowState {
    pub(crate) fn new(limits: AckWindow) -> Self {
        Self {
            limits,
            pending: Mutex::new((VecDeque::new(), 0)),
            notify: Notify::new(),
        }
    }

    pub(crate) fn sent(&self, size: usize) {
        let mut pending = self.pending.lock().unwrap();
        pending.0.push_back(size);
        pending.1 = pending.1.saturating_add(size);
    }

    pub(crate) fn ack(&self) {
        {
            let mut pending = self.pending.lock().unwrap();
            if let Some(size) = pending.0.pop_front() {
                pending.1 -= size;
            }
        }
        self.notify.notify_waiters();
    }

    pub(crate) fn is_full(&self) -> bool {
        let pending = self.pending.lock().unwrap();
        pending.0.len() >= self.limits.max_chunks || pending.1 >= self.limits.max_bytes
    }

    pub(crate) async fn ready(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.is_full() {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn per_chunk_window_waits_for_every_ack() {
        let w = WindowState::new(AckWindow::PER_CHUNK);
        assert!(!w.is_full());
        w.sent(10);
        assert!(w.is_full());
        w.ack();
        assert!(!w.is_full());
        // A stray ack does not bank credit.
        w.ack();
        w.sent(10);
        assert!(w.is_full());
    }

    #[test]
    fn terminal_window_counts_chunks_and_bytes() {
        let w = WindowState::new(AckWindow::TERMINAL);
        for _ in 0..7 {
            w.sent(100);
            assert!(!w.is_full());
        }
        w.sent(100);
        assert!(w.is_full(), "8 chunks fill the window");
        w.ack();
        assert!(!w.is_full());

        let w = WindowState::new(AckWindow::TERMINAL);
        w.sent(40 * 1024);
        assert!(!w.is_full());
        w.sent(24 * 1024);
        assert!(w.is_full(), "64 KiB fills the window");
        w.ack();
        assert!(!w.is_full());
    }

    #[tokio::test]
    async fn ready_wakes_on_ack() {
        let w = Arc::new(WindowState::new(AckWindow::PER_CHUNK));
        w.sent(1);
        let waiter = {
            let w = w.clone();
            tokio::spawn(async move { w.ready().await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        w.ack();
        tokio::time::timeout(Duration::from_secs(1), waiter).await.unwrap().unwrap();
    }
}
