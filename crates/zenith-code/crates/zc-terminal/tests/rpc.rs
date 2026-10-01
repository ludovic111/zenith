//! The terminal RPC methods frame by frame (zc-rpc over in-memory channels, fake PTY):
//! `OutputProtocol.test.ts` (8 chunks / 64 KiB for `terminal.attach` and
//! `subscribeTerminalEvents`, one chunk for `subscribeTerminalMetadata`), plus the unary
//! replies, typed failures, the scope check and payload validation.

use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use zc_rpc::{AuthContext, ConnectionSetup, Inbound, Outbound, RpcRouter, RpcServer};
use zc_terminal::contracts::{TerminalOpenInput, DEFAULT_TERMINAL_ID};
use zc_terminal::rpc::{register, TERMINAL_METHODS, TERMINAL_OPERATE_SCOPE};
use zc_terminal::subprocess::SubprocessInspectResult;
use zc_terminal::testing::FakePtyAdapter;
use zc_terminal::{TerminalManager, TerminalManagerOptions};

struct Harness {
    _dir: tempfile::TempDir,
    pty: Arc<FakePtyAdapter>,
    manager: TerminalManager,
    server: Arc<RpcServer>,
    cwd: String,
}

async fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let pty = Arc::new(FakePtyAdapter::default());
    let mut options = TerminalManagerOptions::new(dir.path().join("logs"), pty.clone());
    options.process_kill_grace = Duration::from_millis(1);
    options.subprocess_poll_interval = Duration::from_secs(3600);
    options.subprocess_inspector = Some(Arc::new(|_| Box::pin(async { Ok(SubprocessInspectResult::default()) })));
    let manager = TerminalManager::new(options).await.unwrap();
    let router = register(RpcRouter::builder(), Arc::new(manager.clone())).build().unwrap();
    let cwd = dir.path().canonicalize().unwrap().to_string_lossy().into_owned();
    Harness {
        _dir: dir,
        pty,
        manager,
        server: RpcServer::new(router),
        cwd,
    }
}

struct Client {
    tx: mpsc::UnboundedSender<Inbound>,
    rx: mpsc::UnboundedReceiver<Outbound>,
}

impl Client {
    fn connect(server: &Arc<RpcServer>, scopes: &[&str]) -> Self {
        let (tx, in_rx) = mpsc::unbounded();
        let (out_tx, rx) = mpsc::unbounded();
        let server = server.clone();
        let setup = ConnectionSetup {
            auth: AuthContext::new(scopes.iter().copied()),
            ..Default::default()
        };
        tokio::spawn(async move {
            server.serve_socket(setup, in_rx, out_tx.sink_map_err(|_| ())).await;
        });
        Self { tx, rx }
    }

    fn send(&self, frame: Value) {
        self.tx.unbounded_send(Inbound::Text(frame.to_string())).unwrap();
    }

    fn request(&self, id: u64, tag: &str, payload: Value) {
        self.send(json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]}));
    }

    fn ack(&self, id: u64) {
        self.send(json!({"_tag":"Ack","requestId":id}));
    }

    async fn recv(&mut self) -> Value {
        match tokio::time::timeout(Duration::from_secs(5), self.rx.next()).await {
            Ok(Some(Outbound::Text(text))) => serde_json::from_str(&text).unwrap(),
            other => panic!("expected a frame, got {other:?}"),
        }
    }

    async fn quiet(&mut self, ms: u64) -> bool {
        tokio::time::timeout(Duration::from_millis(ms), self.rx.next()).await.is_err()
    }
}

const OPERATE: &[&str] = &[TERMINAL_OPERATE_SCOPE];

fn chunk_values(frame: &Value) -> &Vec<Value> {
    assert_eq!(frame["_tag"], "Chunk", "{frame}");
    frame["values"].as_array().unwrap()
}

/// Opens the terminal and attaches (or subscribes) as request 1; returns the first chunk.
async fn start_stream(h: &Harness, c: &mut Client, tag: &str) -> Value {
    h.manager.open(TerminalOpenInput::new("thread-1", DEFAULT_TERMINAL_ID, &h.cwd)).await.unwrap();
    let payload = if tag == "terminal.attach" {
        json!({"threadId":"thread-1","terminalId":DEFAULT_TERMINAL_ID})
    } else {
        json!({})
    };
    c.request(1, tag, payload);
    if tag == "terminal.attach" {
        c.recv().await
    } else {
        Value::Null
    }
}

/// One output per chunk: emit, then wait for its frame.
async fn emit_and_receive(h: &Harness, c: &mut Client, data: &str) -> Value {
    h.pty.process(0).emit_data(data);
    let frame = c.recv().await;
    let values = chunk_values(&frame);
    assert_eq!(values.len(), 1);
    assert_eq!(values[0]["type"], "output");
    assert_eq!(values[0]["data"], data);
    frame
}

async fn check_chunk_window(tag: &str) {
    let h = harness().await;
    let mut c = Client::connect(&h.server, OPERATE);
    let first = start_stream(&h, &mut c, tag).await;
    let mut outstanding = 0;
    if tag == "terminal.attach" {
        assert_eq!(chunk_values(&first)[0]["type"], "snapshot");
        outstanding += 1;
    } else {
        // Let the subscription register before output flows.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    while outstanding < 8 {
        emit_and_receive(&h, &mut c, &outstanding.to_string()).await;
        outstanding += 1;
    }
    // The window is full: the next output waits for an ack.
    h.pty.process(0).emit_data("i");
    assert!(c.quiet(150).await, "{tag}: a 9th chunk went out without an ack");
    c.ack(1);
    let resumed = c.recv().await;
    assert_eq!(chunk_values(&resumed)[0]["data"], "i");
    h.pty.process(0).emit_data("j");
    assert!(c.quiet(150).await, "{tag}: the window is full again");
    c.send(json!({"_tag":"Interrupt","requestId":1}));
    let exit = c.recv().await;
    assert_eq!(exit["_tag"], "Exit");
    assert_eq!(exit["exit"]["cause"][0]["_tag"], "Interrupt");
}

#[tokio::test]
async fn terminal_attach_allows_8_unacknowledged_chunks() {
    check_chunk_window("terminal.attach").await;
}

#[tokio::test]
async fn subscribe_terminal_events_allows_8_unacknowledged_chunks() {
    check_chunk_window("subscribeTerminalEvents").await;
}

#[tokio::test]
async fn terminal_attach_window_is_also_bounded_by_64_kib() {
    let h = harness().await;
    let mut c = Client::connect(&h.server, OPERATE);
    start_stream(&h, &mut c, "terminal.attach").await;
    let big = "x".repeat(64 * 1024);
    emit_and_receive(&h, &mut c, &big).await;
    h.pty.process(0).emit_data("i");
    assert!(c.quiet(150).await, "64 KiB unacknowledged fills the window");
    // The first ack only retires the snapshot chunk: 64 KiB are still outstanding.
    c.ack(1);
    assert!(c.quiet(150).await);
    c.ack(1);
    assert_eq!(chunk_values(&c.recv().await)[0]["data"], "i");
}

#[tokio::test]
async fn subscribe_terminal_metadata_waits_for_each_ack() {
    let h = harness().await;
    let mut c = Client::connect(&h.server, OPERATE);
    c.request(1, "subscribeTerminalMetadata", json!({}));
    let snapshot = c.recv().await;
    assert_eq!(chunk_values(&snapshot)[0], json!({"type":"snapshot","terminals":[]}));
    h.manager.open(TerminalOpenInput::new("thread-1", DEFAULT_TERMINAL_ID, &h.cwd)).await.unwrap();
    assert!(c.quiet(150).await, "metadata keeps Effect's one-chunk latch");
    c.ack(1);
    let upsert = c.recv().await;
    let values = chunk_values(&upsert);
    assert_eq!(values[0]["type"], "upsert");
    assert_eq!(values[0]["terminal"]["threadId"], "thread-1");
    assert_eq!(values[0]["terminal"]["hasRunningSubprocess"], false);
}

#[tokio::test]
async fn unary_methods_reply_like_the_ts_server() {
    let h = harness().await;
    let mut c = Client::connect(&h.server, OPERATE);

    c.request(
        1,
        "terminal.open",
        json!({"threadId":"thread-1","terminalId":"term-1","cwd":h.cwd,"cols":80,"rows":24}),
    );
    let exit = c.recv().await;
    let snapshot = &exit["exit"]["value"];
    assert_eq!(exit["exit"]["_tag"], "Success");
    assert_eq!(snapshot["status"], "running");
    assert_eq!(snapshot["pid"], 9000);
    assert_eq!(snapshot["worktreePath"], Value::Null);
    assert_eq!(snapshot["exitCode"], Value::Null);
    assert_eq!(snapshot["sequence"], 1);
    assert_eq!(snapshot["label"], "Terminal 1");

    c.request(2, "terminal.write", json!({"threadId":"thread-1","terminalId":"term-1","data":"ls\n"}));
    assert_eq!(c.recv().await, json!({"_tag":"Exit","requestId":2,"exit":{"_tag":"Success","value":null}}));
    assert_eq!(h.pty.process(0).writes(), ["ls\n"]);

    c.request(3, "terminal.resize", json!({"threadId":"thread-1","terminalId":"term-1","cols":100,"rows":30}));
    assert_eq!(c.recv().await["exit"]["_tag"], "Success");
    assert_eq!(h.pty.process(0).resize_calls(), [(100, 30)]);

    c.request(4, "terminal.write", json!({"threadId":"thread-1","terminalId":"nope","data":"x"}));
    assert_eq!(
        c.recv().await,
        json!({"_tag":"Exit","requestId":4,"exit":{"_tag":"Failure","cause":[{"_tag":"Fail","error":{
            "_tag":"TerminalSessionLookupError","threadId":"thread-1","terminalId":"nope"}}]}})
    );

    // Payload validation happens before the handler: a per-request Die, never a Defect.
    c.request(5, "terminal.resize", json!({"threadId":"thread-1","terminalId":"term-1","cols":0,"rows":30}));
    let exit = c.recv().await;
    assert_eq!(exit["exit"]["cause"][0]["_tag"], "Die");
    assert!(exit["exit"]["cause"][0]["defect"].is_string());
    c.request(
        6,
        "terminal.write",
        json!({"threadId":"thread-1","terminalId":"term-1","data":"x".repeat(65_537)}),
    );
    assert_eq!(c.recv().await["exit"]["cause"][0]["_tag"], "Die");

    c.request(
        7,
        "terminal.restart",
        json!({"threadId":"thread-1","terminalId":"term-1","cwd":h.cwd,"cols":80,"rows":24}),
    );
    let exit = c.recv().await;
    assert_eq!(exit["exit"]["value"]["pid"], 9001);
    assert_eq!(exit["exit"]["value"]["history"], "");

    c.request(8, "terminal.clear", json!({"threadId":"thread-1","terminalId":"term-1"}));
    assert_eq!(c.recv().await["exit"]["_tag"], "Success");

    c.request(9, "terminal.close", json!({"threadId":"thread-1","deleteHistory":true}));
    assert_eq!(c.recv().await["exit"]["_tag"], "Success");
    assert!(h.pty.process(1).killed());

    c.request(
        10,
        "terminal.open",
        json!({"threadId":"thread-1","terminalId":"term-1","cwd":format!("{}/missing", h.cwd)}),
    );
    let exit = c.recv().await;
    assert_eq!(exit["exit"]["cause"][0]["error"]["_tag"], "TerminalCwdNotFoundError");
}

#[tokio::test]
async fn every_method_requires_terminal_operate() {
    let h = harness().await;
    let mut c = Client::connect(&h.server, &["orchestration:read"]);
    for (index, (tag, _)) in TERMINAL_METHODS.iter().enumerate() {
        c.request(
            index as u64 + 1,
            tag,
            json!({"threadId":"t","terminalId":"x","cwd":"/","data":"x","cols":1,"rows":1}),
        );
        let exit = c.recv().await;
        assert_eq!(
            exit["exit"]["cause"][0]["error"],
            json!({"_tag":"EnvironmentAuthorizationError",
                   "message":"The authenticated token is missing required scope: terminal:operate.",
                   "requiredScope":"terminal:operate"}),
            "{tag}"
        );
    }
    for (tag, stream) in TERMINAL_METHODS {
        assert_eq!(h.server.router().is_stream(tag), Some(stream), "{tag}");
    }
}
