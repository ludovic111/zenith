//! Dummy handlers for the RPC conformance suite: the Rust side of the test `RpcGroup`
//! in `code/scripts/rpc-conformance/conformance.mjs`, which drives this server with the
//! real Effect RPC client. Served by `zenith-code dev-serve --conformance`.
//!
//! Every method needs `orchestration:read` (which [`scopes`] grants) except
//! `conformance.forbidden`, which needs `access:write` (which it does not).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use zc_rpc::{AckWindow, Failure, MethodOptions, RpcError, RpcMethod, RpcRouter, ScopeTable};

/// The scopes the dev authenticator grants in conformance mode.
pub fn scopes() -> Vec<String> {
    vec!["orchestration:read".into()]
}

const METHODS: &[(&str, &str)] = &[
    ("conformance.echo", "orchestration:read"),
    ("conformance.fail", "orchestration:read"),
    ("conformance.die", "orchestration:read"),
    ("conformance.panic", "orchestration:read"),
    ("conformance.forbidden", "access:write"),
    ("conformance.decode", "orchestration:read"),
    ("conformance.count", "orchestration:read"),
    ("conformance.failAfter", "orchestration:read"),
    ("conformance.ticker", "orchestration:read"),
    ("conformance.windowed", "orchestration:read"),
    ("conformance.probe", "orchestration:read"),
];

#[derive(Deserialize)]
pub struct EchoInput {
    text: String,
}

#[derive(Serialize)]
pub struct EchoResult {
    text: String,
    length: usize,
}

/// `ConformanceError`, the group's typed error.
#[derive(Serialize, Debug)]
#[serde(tag = "_tag")]
pub enum ConformanceError {
    ConformanceError { message: String, code: u32 },
}

fn conformance_error(message: &str) -> ConformanceError {
    ConformanceError::ConformanceError {
        message: message.into(),
        code: 7,
    }
}

pub struct Echo;
impl RpcMethod for Echo {
    const TAG: &'static str = "conformance.echo";
    const STREAM: bool = false;
    type Payload = EchoInput;
    type Success = EchoResult;
    type Error = ConformanceError;
}

#[derive(Deserialize)]
pub struct DecodeInput {
    n: u32,
}

pub struct Decode;
impl RpcMethod for Decode {
    const TAG: &'static str = "conformance.decode";
    const STREAM: bool = false;
    type Payload = DecodeInput;
    type Success = u32;
    type Error = ConformanceError;
}

#[derive(Deserialize)]
pub struct FailAfterInput {
    count: u32,
}

#[derive(Serialize)]
pub struct Item {
    index: u64,
}

pub struct FailAfter;
impl RpcMethod for FailAfter {
    const TAG: &'static str = "conformance.failAfter";
    const STREAM: bool = true;
    type Payload = FailAfterInput;
    type Success = Item;
    type Error = ConformanceError;
}

/// What the streams did, by the key the client gave them; read back with
/// `conformance.probe`.
#[derive(Default)]
struct Probes {
    produced: Mutex<HashMap<String, u64>>,
    cancelled: Mutex<HashMap<String, bool>>,
}

impl Probes {
    fn produced(&self, key: &str) {
        *self.produced.lock().unwrap().entry(key.to_owned()).or_default() += 1;
    }
}

/// Marks a stream as cancelled when the runtime drops it before the end.
struct DropGuard {
    probes: Arc<Probes>,
    key: String,
    finished: bool,
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.probes.cancelled.lock().unwrap().insert(self.key.clone(), true);
        }
    }
}

fn key_of(payload: &Value) -> String {
    payload["key"].as_str().unwrap_or("default").to_owned()
}

/// A stream of `{index}` items. With `spaced`, it waits a scheduler turn (or
/// `interval_ms`) before each item, so each one is its own chunk; without, everything
/// is ready at once and gets batched.
fn items(
    probes: Arc<Probes>,
    key: String,
    count: Option<u64>,
    interval_ms: u64,
    spaced: bool,
    pad: usize,
) -> impl Stream<Item = Result<Value, RpcError>> + Send {
    let guard = DropGuard {
        probes: probes.clone(),
        key: key.clone(),
        finished: false,
    };
    futures::stream::unfold((0u64, guard), move |(index, mut guard)| {
        let probes = probes.clone();
        let key = key.clone();
        async move {
            if count.is_some_and(|c| index >= c) {
                guard.finished = true;
                drop(guard);
                return None;
            }
            if interval_ms > 0 {
                tokio::time::sleep(Duration::from_millis(interval_ms)).await;
            } else if spaced {
                tokio::task::yield_now().await;
            }
            probes.produced(&key);
            let item = if pad > 0 {
                json!({"index": index, "pad": "x".repeat(pad)})
            } else {
                json!({"index": index})
            };
            Some((Ok(item), (index + 1, guard)))
        }
    })
}

/// The conformance method table.
pub fn router() -> RpcRouter {
    let probes = Arc::new(Probes::default());
    let (p1, p2, p3, p4) = (probes.clone(), probes.clone(), probes.clone(), probes);
    RpcRouter::builder()
        .scopes(ScopeTable::from_iter(METHODS.iter().copied()))
        .typed_unary::<Echo, _, _>(|_, input| async move {
            Ok(EchoResult {
                length: input.text.chars().count(),
                text: input.text,
            })
        })
        .unary(
            "conformance.fail",
            |_, _| async move { Err(RpcError::fail(conformance_error("requested failure"))) },
        )
        .unary("conformance.die", |_, _| async move { Err(RpcError::die("conformance die")) })
        .unary("conformance.panic", |_, _| async move {
            if true {
                panic!("conformance panic");
            }
            Ok(Value::Null)
        })
        .unary("conformance.forbidden", |_, _| async move { Ok(json!("should not run")) })
        .typed_unary::<Decode, _, _>(|_, input| async move { Ok(input.n * 2) })
        .stream("conformance.count", move |_, payload| {
            let probes = p1.clone();
            async move {
                let count = payload["count"].as_u64().unwrap_or(10);
                let spaced = payload["spaced"].as_bool().unwrap_or(true);
                Ok(items(probes, key_of(&payload), Some(count), 0, spaced, 0))
            }
        })
        .typed_stream::<FailAfter, _, _, _>(|_, input| async move {
            let count = u64::from(input.count);
            let items = futures::stream::iter(0..count)
                .then(|index| async move {
                    tokio::task::yield_now().await;
                    Ok::<Item, Failure<ConformanceError>>(Item { index })
                })
                .chain(futures::stream::once(async { Err(Failure::Fail(conformance_error("stream failed"))) }));
            Ok(items)
        })
        .stream("conformance.ticker", move |_, payload| {
            let probes = p2.clone();
            async move {
                let interval = payload["intervalMs"].as_u64().unwrap_or(10).max(1);
                Ok(items(probes, key_of(&payload), None, interval, true, 0))
            }
        })
        .stream_with(
            "conformance.windowed",
            MethodOptions::default().ack_window(AckWindow::TERMINAL),
            move |_, payload| {
                let probes = p3.clone();
                async move {
                    let count = payload["count"].as_u64();
                    let pad = payload["pad"].as_u64().unwrap_or(0) as usize;
                    Ok(items(probes, key_of(&payload), count, 0, true, pad))
                }
            },
        )
        .unary("conformance.probe", move |_, payload| {
            let probes = p4.clone();
            async move {
                let key = key_of(&payload);
                let produced = probes.produced.lock().unwrap().get(&key).copied().unwrap_or(0);
                let cancelled = probes.cancelled.lock().unwrap().get(&key).copied().unwrap_or(false);
                Ok(json!({"produced": produced, "cancelled": cancelled}))
            }
        })
        .build()
        .expect("the conformance router is well-formed")
}
