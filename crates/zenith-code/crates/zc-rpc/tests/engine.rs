//! The protocol engine driven over in-memory channels, frame by frame.

use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use zc_rpc::{
    AckWindow, AuthContext, ConnectionSetup, Failure, Inbound, MethodOptions, Outbound, RpcError, RpcMethod, RpcRouter, RpcServer, ScopeRule, ScopeTable,
};

#[derive(Deserialize)]
struct AddInput {
    a: i64,
    b: i64,
}

#[derive(Serialize, Debug)]
#[serde(tag = "_tag")]
enum AddError {
    Overflow { message: String },
}

struct Add;
impl RpcMethod for Add {
    const TAG: &'static str = "math.add";
    const STREAM: bool = false;
    type Payload = AddInput;
    type Success = i64;
    type Error = AddError;
}

struct Count;
impl RpcMethod for Count {
    const TAG: &'static str = "math.count";
    const STREAM: bool = true;
    type Payload = u32;
    type Success = u32;
    type Error = AddError;
}

fn router() -> RpcRouter {
    RpcRouter::builder()
        .scopes(ScopeTable::from_iter([
            ("echo", "orchestration:read"),
            ("secret", "access:write"),
            ("math.add", "orchestration:read"),
            ("math.count", "orchestration:read"),
            ("never", "orchestration:read"),
            ("boom", "orchestration:read"),
            ("ticks", "orchestration:read"),
            ("term", "terminal:operate"),
        ]))
        .unary("echo", |_, payload| async move { Ok(payload) })
        .unary("secret", |_, _| async move { Ok(json!("no")) })
        .unary("never", |_, _| async move {
            futures::future::pending::<()>().await;
            Ok(Value::Null)
        })
        .unary("boom", |_, _| async move {
            if true {
                panic!("kaboom");
            }
            Ok(Value::Null)
        })
        .typed_unary::<Add, _, _>(|_, input| async move {
            input
                .a
                .checked_add(input.b)
                .ok_or_else(|| Failure::Fail(AddError::Overflow { message: "too big".into() }))
        })
        .typed_stream::<Count, _, _, _>(|_, n| async move { Ok(futures::stream::iter((0..n).map(Ok::<u32, Failure<AddError>>))) })
        .stream("ticks", |_, payload| async move {
            let n = payload["n"].as_u64().unwrap_or(3);
            Ok(futures::stream::unfold(0u64, move |i| async move {
                if i >= n {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
                Some((Ok::<Value, RpcError>(json!(i)), i + 1))
            }))
        })
        .stream_with("term", MethodOptions::default().ack_window(AckWindow::TERMINAL), |_, _| async move {
            Ok(futures::stream::unfold(0u64, |i| async move {
                tokio::time::sleep(Duration::from_millis(1)).await;
                Some((Ok::<Value, RpcError>(json!(i)), i + 1))
            }))
        })
        .unary_with(
            "dyn",
            MethodOptions::default().scope(ScopeRule::dynamic(|p: &Value| {
                if p.get("retryHostId").is_some() {
                    "orchestration:operate"
                } else {
                    "orchestration:read"
                }
                .into()
            })),
            |_, _| async move { Ok(json!("ok")) },
        )
        .build()
        .unwrap()
}

struct Client {
    tx: mpsc::UnboundedSender<Inbound>,
    rx: mpsc::UnboundedReceiver<Outbound>,
    done: tokio::task::JoinHandle<()>,
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
        let done = tokio::spawn(async move {
            server.serve_socket(setup, in_rx, out_tx.sink_map_err(|_| ())).await;
        });
        Self { tx, rx, done }
    }

    fn send(&self, frame: Value) {
        self.tx.unbounded_send(Inbound::Text(frame.to_string())).unwrap();
    }

    fn send_raw(&self, text: &str) {
        self.tx.unbounded_send(Inbound::Text(text.into())).unwrap();
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

const ALL: &[&str] = &["orchestration:read", "terminal:operate"];

#[tokio::test]
async fn unary_success_and_id_types() {
    let server = RpcServer::new(router());
    let mut c = Client::connect(&server, ALL);
    c.send(json!({"_tag":"Request","id":1,"tag":"echo","payload":{"x":[1,2]},"headers":[]}));
    assert_eq!(
        c.recv().await,
        json!({"_tag":"Exit","requestId":1,"exit":{"_tag":"Success","value":{"x":[1,2]}}})
    );
    c.send(json!({"_tag":"Request","id":"s1","tag":"echo","payload":null,"headers":[]}));
    assert_eq!(c.recv().await, json!({"_tag":"Exit","requestId":"s1","exit":{"_tag":"Success","value":null}}));
}

#[tokio::test]
async fn per_request_failures_never_defect() {
    let server = RpcServer::new(router());
    let mut c = Client::connect(&server, ALL);
    c.send(json!({"_tag":"Request","id":1,"tag":"nope","payload":{},"headers":[]}));
    assert_eq!(
        c.recv().await,
        json!({"_tag":"Exit","requestId":1,"exit":{"_tag":"Failure","cause":[{"_tag":"Die","defect":"Unknown request tag: nope"}]}})
    );
    c.send(json!({"_tag":"Request","id":2,"tag":"secret","payload":{},"headers":[]}));
    assert_eq!(
        c.recv().await,
        json!({"_tag":"Exit","requestId":2,"exit":{"_tag":"Failure","cause":[{"_tag":"Fail","error":{
            "_tag":"EnvironmentAuthorizationError",
            "message":"The authenticated token is missing required scope: access:write.",
            "requiredScope":"access:write"}}]}})
    );
    c.send(json!({"_tag":"Request","id":3,"tag":"math.add","payload":{"a":"x"},"headers":[]}));
    let exit = c.recv().await;
    assert_eq!(exit["exit"]["cause"][0]["_tag"], "Die");
    assert!(exit["exit"]["cause"][0]["defect"].is_string());
    c.send(json!({"_tag":"Request","id":4,"tag":"math.add","payload":{"a":i64::MAX,"b":1},"headers":[]}));
    assert_eq!(
        c.recv().await,
        json!({"_tag":"Exit","requestId":4,"exit":{"_tag":"Failure","cause":[{"_tag":"Fail","error":{"_tag":"Overflow","message":"too big"}}]}})
    );
    c.send(json!({"_tag":"Request","id":5,"tag":"boom","payload":{},"headers":[]}));
    assert_eq!(
        c.recv().await,
        json!({"_tag":"Exit","requestId":5,"exit":{"_tag":"Failure","cause":[{"_tag":"Die","defect":{"name":"Error","message":"kaboom"}}]}})
    );
    c.send(json!({"_tag":"Request","id":6,"tag":"dyn","payload":{"retryHostId":"h"},"headers":[]}));
    assert_eq!(c.recv().await["exit"]["cause"][0]["error"]["requiredScope"], "orchestration:operate");
    c.send(json!({"_tag":"Request","id":7,"tag":"dyn","payload":{},"headers":[]}));
    assert_eq!(c.recv().await["exit"]["value"], "ok");
}

#[tokio::test]
async fn protocol_errors_are_defects_and_arrays_work() {
    let server = RpcServer::new(router());
    let mut c = Client::connect(&server, ALL);
    c.send_raw("{not json");
    let defect = c.recv().await;
    assert_eq!(defect["_tag"], "Defect");
    assert_eq!(defect["defect"]["name"], "SyntaxError");
    c.send(json!({"_tag":"Request","id":{},"tag":"echo"}));
    assert_eq!(c.recv().await, json!({"_tag":"Defect","defect":"Invalid request id: [object Object]"}));
    c.send(json!([{"_tag":"Ping"},{"_tag":"Request","id":9,"tag":"echo","payload":1,"headers":[]}]));
    assert_eq!(c.recv().await, json!({"_tag":"Pong"}));
    assert_eq!(c.recv().await["requestId"], 9);
}

#[tokio::test]
async fn stream_waits_for_each_ack() {
    let server = RpcServer::new(router());
    let mut c = Client::connect(&server, ALL);
    c.send(json!({"_tag":"Request","id":1,"tag":"ticks","payload":{"n":3},"headers":[]}));
    let mut seen = Vec::new();
    loop {
        let frame = c.recv().await;
        if frame["_tag"] == "Exit" {
            assert_eq!(frame["exit"], json!({"_tag":"Success","value":null}));
            break;
        }
        assert_eq!(frame["_tag"], "Chunk");
        let values = frame["values"].as_array().unwrap().clone();
        assert!(!values.is_empty());
        seen.extend(values);
        // Nothing more until we acknowledge.
        assert!(c.quiet(50).await);
        c.send(json!({"_tag":"Ack","requestId":1}));
    }
    assert_eq!(seen, vec![json!(0), json!(1), json!(2)]);
}

#[tokio::test]
async fn ready_values_are_batched() {
    let server = RpcServer::new(router());
    let mut c = Client::connect(&server, ALL);
    c.send(json!({"_tag":"Request","id":1,"tag":"math.count","payload":5,"headers":[]}));
    assert_eq!(c.recv().await, json!({"_tag":"Chunk","requestId":1,"values":[0,1,2,3,4]}));
    c.send(json!({"_tag":"Ack","requestId":1}));
    assert_eq!(c.recv().await, json!({"_tag":"Exit","requestId":1,"exit":{"_tag":"Success","value":null}}));
}

#[tokio::test]
async fn windowed_stream_runs_eight_chunks_ahead() {
    let server = RpcServer::new(router());
    let mut c = Client::connect(&server, ALL);
    c.send(json!({"_tag":"Request","id":1,"tag":"term","payload":{},"headers":[]}));
    for _ in 0..8 {
        assert_eq!(c.recv().await["_tag"], "Chunk");
    }
    assert!(c.quiet(100).await, "the window is full after 8 chunks");
    c.send(json!({"_tag":"Ack","requestId":1}));
    assert_eq!(c.recv().await["_tag"], "Chunk");
    assert!(c.quiet(100).await);
    // Ping is answered while the stream is blocked.
    c.send(json!({"_tag":"Ping"}));
    assert_eq!(c.recv().await, json!({"_tag":"Pong"}));
    c.send(json!({"_tag":"Interrupt","requestId":1}));
    assert_eq!(
        c.recv().await,
        json!({"_tag":"Exit","requestId":1,"exit":{"_tag":"Failure","cause":[{"_tag":"Interrupt","fiberId":null}]}})
    );
}

#[tokio::test]
async fn interrupt_cancels_a_unary_handler() {
    let server = RpcServer::new(router());
    let mut c = Client::connect(&server, ALL);
    c.send(json!({"_tag":"Request","id":1,"tag":"never","payload":{},"headers":[]}));
    assert!(c.quiet(50).await);
    c.send(json!({"_tag":"Interrupt","requestId":1}));
    assert_eq!(c.recv().await["exit"]["cause"][0]["_tag"], "Interrupt");
    // An interrupt for a finished request gets no reply.
    c.send(json!({"_tag":"Interrupt","requestId":1}));
    assert!(c.quiet(50).await);
}

#[tokio::test]
async fn closing_the_socket_cancels_everything() {
    let server = RpcServer::new(router());
    let c = Client::connect(&server, ALL);
    c.send(json!({"_tag":"Request","id":1,"tag":"never","payload":{},"headers":[]}));
    c.send(json!({"_tag":"Request","id":2,"tag":"term","payload":{},"headers":[]}));
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(server.connection_count(), 1);
    c.tx.unbounded_send(Inbound::Close).unwrap();
    tokio::time::timeout(Duration::from_secs(2), c.done).await.unwrap().unwrap();
    assert_eq!(server.connection_count(), 0);
}

#[tokio::test]
async fn shutdown_closes_sockets_with_1001() {
    let server = RpcServer::new(router());
    let mut c = Client::connect(&server, ALL);
    c.send(json!({"_tag":"Request","id":1,"tag":"never","payload":{},"headers":[]}));
    tokio::time::sleep(Duration::from_millis(30)).await;
    tokio::time::timeout(Duration::from_secs(2), server.shutdown()).await.unwrap();
    let mut close = None;
    while let Some(frame) = c.rx.next().await {
        if let Outbound::Close { code, .. } = frame {
            close = Some(code);
        }
    }
    assert_eq!(close, Some(1001));
    // Late sockets are turned away.
    let mut late = Client::connect(&server, ALL);
    assert!(matches!(late.rx.next().await, Some(Outbound::Close { code: 1001, .. })));
}

#[tokio::test]
async fn many_concurrent_requests() {
    let server = RpcServer::new(router());
    let mut c = Client::connect(&server, ALL);
    for i in 0..200 {
        c.send(json!({"_tag":"Request","id":i,"tag":"math.add","payload":{"a":i,"b":1},"headers":[]}));
    }
    let mut got = std::collections::BTreeMap::new();
    for _ in 0..200 {
        let f = c.recv().await;
        got.insert(f["requestId"].as_i64().unwrap(), f["exit"]["value"].as_i64().unwrap());
    }
    assert_eq!(got.len(), 200);
    assert!(got.iter().all(|(k, v)| *v == k + 1));
}
