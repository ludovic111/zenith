//! Replay gate: real provider runtime events (the `CANON:` lines of a provider event log)
//! through the Rust ingestion into a fresh engine give the same domain events as the
//! TypeScript ingestion on the same input.
//!
//! ```sh
//! cp ~/.zenith/code/userdata/logs/provider/events.<thread>.log /tmp/replay/   # copies only
//! cd code
//! node apps/server/scripts/ingestion-oracle.ts script /tmp/replay/events.<thread>.log --out /tmp/replay/<name>.script.jsonl
//! node apps/server/scripts/ingestion-oracle.ts run /tmp/replay/<name>.script.jsonl --out /tmp/replay/<name>.ts.json
//! ZC_INGESTION_REPLAY_DIR=/tmp/replay cargo test -p zc-reactors --test ingestion_replay_gate -- --ignored --nocapture
//! ```
//!
//! Every `<name>.script.jsonl` with a `<name>.ts.json` beside it is replayed. Both sides read
//! one virtual clock that the script moves to each event's `createdAt` (so the 400 ms
//! streaming cadence and every "now" come from the script), and both drain after every step.
//! Generated ids (event ids, the uuid tail of command ids, generated message ids) are renamed
//! to `<uuid-N>` in order of first appearance before comparing.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use futures::StreamExt;
use regex::Regex;
use serde_json::{json, Value};
use zc_db::Db;
use zc_orchestration::decider::DeciderEnv;
use zc_orchestration::engine::{EngineConfig, EventLogReads, OrchestrationEngine};
use zc_orchestration::pipeline::NoopProjectionPipeline;
use zc_ports::OrchestrationDispatch;
use zc_reactors::common::system_uuids;
use zc_reactors::registries::{ThreadBackgroundLivenessRegistry, ThreadPlanProgressRegistry};
use zc_reactors::{EventLogReactorReads, FixedRepositoryProbe, IngestionDeps, ManualClock, ProviderRuntimeIngestion, ReactorClock};

/// The decider's "now" on the replay clock.
struct ReplayEnv(Arc<ManualClock>);

impl DeciderEnv for ReplayEnv {
    fn now_iso(&self) -> String {
        self.0.now_iso()
    }
    fn new_event_id(&self) -> String {
        zc_core::ids::uuid_v4()
    }
}

/// Replays one script; returns the stored domain events (wire JSON) and dispatch errors.
async fn replay(script: &str) -> (Vec<Value>, Vec<String>) {
    let clock = Arc::new(ManualClock::fixed(0));
    let liveness = Arc::new(ThreadBackgroundLivenessRegistry::new());
    let db = Db::open_in_memory().expect("in-memory database");
    let engine = OrchestrationEngine::start(EngineConfig {
        db: db.clone(),
        reads: Arc::new(EventLogReads::new(db.clone())),
        pipeline: Arc::new(NoopProjectionPipeline),
        liveness: liveness.clone(),
        env: Arc::new(ReplayEnv(clock.clone())),
    })
    .await
    .expect("start the engine");
    let engine: Arc<dyn OrchestrationDispatch> = Arc::new(engine);
    let reads = Arc::new(EventLogReactorReads::new(engine.clone()));
    let providers = FakeProviders::new();
    let ingestion = ProviderRuntimeIngestion::new(
        IngestionDeps {
            engine: engine.clone(),
            reads,
            providers: providers.clone(),
            settings: MemorySettings::new(json!({})),
            repositories: Arc::new(FixedRepositoryProbe(false)),
            liveness,
            plan_progress: Arc::new(ThreadPlanProgressRegistry::new()),
            clock: clock.clone(),
            uuids: system_uuids(),
        },
        tokio_util::sync::CancellationToken::new(),
    );
    ingestion.start();

    let mut errors = Vec::new();
    for (index, line) in script.lines().filter(|line| !line.trim().is_empty()).enumerate() {
        let step: Value = serde_json::from_str(line).expect("script step");
        match step["kind"].as_str() {
            Some("clock") => clock.set(step["millis"].as_i64().expect("clock millis")),
            Some("session") => providers.set_session(step["session"].clone()),
            Some("command") => {
                if let Err(error) = dispatch(&*engine, step["command"].clone()).await {
                    errors.push(format!("step {index}: {}: {}", error.tag, error.message));
                }
                // The ingestion reacts to `thread.turn-start-requested` on its subscription.
                tokio::time::sleep(Duration::from_millis(5)).await;
                ingestion.drain().await;
            }
            Some("event") => {
                ingestion.enqueue_runtime_event(step["event"].clone());
                ingestion.drain().await;
            }
            other => panic!("unknown step kind {other:?}"),
        }
    }
    tokio::time::sleep(Duration::from_millis(5)).await;
    ingestion.drain().await;
    ingestion.stop();

    let events = engine
        .read_events(0, None)
        .map(|event| serde_json::to_value(event.expect("stored event")).expect("encode event"))
        .collect::<Vec<_>>()
        .await;
    (events, errors)
}

/// Sorts object keys and renames every uuid the script does not contain to `<uuid-N>`, in
/// order of first appearance.
struct Normalizer {
    pattern: Regex,
    known: std::collections::HashSet<String>,
    names: HashMap<String, String>,
}

impl Normalizer {
    fn new(script: &str) -> Self {
        let pattern = Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap();
        let known = pattern.find_iter(script).map(|found| found.as_str().to_owned()).collect();
        Self {
            pattern,
            known,
            names: HashMap::new(),
        }
    }

    fn value(&mut self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.text(text)),
            Value::Array(items) => Value::Array(items.iter().map(|item| self.value(item)).collect()),
            Value::Object(object) => {
                let sorted: BTreeMap<&String, &Value> = object.iter().collect();
                let mut out = serde_json::Map::new();
                for (key, item) in sorted {
                    out.insert(key.clone(), self.value(item));
                }
                Value::Object(out)
            }
            other => other.clone(),
        }
    }

    fn text(&mut self, text: &str) -> String {
        let matches: Vec<String> = self.pattern.find_iter(text).map(|found| found.as_str().to_owned()).collect();
        let mut out = text.to_owned();
        for uuid in matches {
            if self.known.contains(&uuid) {
                continue;
            }
            let next = self.names.len() + 1;
            let name = self.names.entry(uuid.clone()).or_insert_with(|| format!("<uuid-{next}>")).clone();
            out = out.replace(&uuid, &name);
        }
        out
    }
}

fn diff(path: &str, expected: &Value, actual: &Value, out: &mut Vec<String>) {
    match (expected, actual) {
        (Value::Object(a), Value::Object(b)) => {
            for (key, value) in a {
                match b.get(key) {
                    Some(other) => diff(&format!("{path}.{key}"), value, other, out),
                    None => out.push(format!("{path}.{key}: missing in Rust (TS {value})")),
                }
            }
            for key in b.keys().filter(|key| !a.contains_key(*key)) {
                out.push(format!("{path}.{key}: only in Rust ({})", b[key]));
            }
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (index, (x, y)) in a.iter().zip(b).enumerate() {
                diff(&format!("{path}[{index}]"), x, y, out);
            }
        }
        _ if expected == actual => {}
        _ => {
            let show = |value: &Value| {
                let text = value.to_string();
                if text.len() > 300 {
                    format!("{}…", &text[..text.floor_char_boundary(300)])
                } else {
                    text
                }
            };
            out.push(format!("{path}: TS {} / Rust {}", show(expected), show(actual)));
        }
    }
}

fn pairs(dir: &Path) -> Vec<(String, PathBuf, PathBuf)> {
    let mut pairs: Vec<(String, PathBuf, PathBuf)> = std::fs::read_dir(dir)
        .expect("read the replay directory")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_str()?.strip_suffix(".script.jsonl")?.to_owned();
            let expected = dir.join(format!("{name}.ts.json"));
            expected.exists().then_some((name, path, expected))
        })
        .collect();
    pairs.sort();
    pairs
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs ZC_INGESTION_REPLAY_DIR (see the module docs)"]
async fn replaying_real_provider_events_matches_the_typescript_ingestion() {
    let dir = PathBuf::from(std::env::var("ZC_INGESTION_REPLAY_DIR").expect("ZC_INGESTION_REPLAY_DIR"));
    let pairs = pairs(&dir);
    assert!(!pairs.is_empty(), "no <name>.script.jsonl with a <name>.ts.json in {}", dir.display());
    let mut failed = Vec::new();
    for (name, script_path, expected_path) in pairs {
        let script = std::fs::read_to_string(&script_path).expect("read the script");
        let expected: Value = serde_json::from_slice(&std::fs::read(&expected_path).expect("read the TS events")).expect("parse the TS events");
        let started = std::time::Instant::now();
        let (actual, errors) = replay(&script).await;
        let elapsed = started.elapsed();

        let mut ts_names = Normalizer::new(&script);
        let mut rust_names = Normalizer::new(&script);
        let expected_events: Vec<Value> = expected["events"]
            .as_array()
            .expect("events")
            .iter()
            .map(|event| ts_names.value(event))
            .collect();
        let actual_events: Vec<Value> = actual.iter().map(|event| rust_names.value(event)).collect();

        let mut differences = Vec::new();
        if expected["errors"].as_array().map_or(0, Vec::len) != errors.len() {
            differences.push(format!("dispatch errors: TS {} / Rust {errors:?}", expected["errors"]));
        }
        if expected_events.len() != actual_events.len() {
            differences.push(format!("event count: TS {} / Rust {}", expected_events.len(), actual_events.len()));
        }
        for (index, (ts, rust)) in expected_events.iter().zip(&actual_events).enumerate() {
            let label = format!("event[{index}] {}", ts["type"].as_str().unwrap_or("?"));
            diff(&label, ts, rust, &mut differences);
        }
        let steps = script.lines().filter(|line| line.contains("\"kind\":\"event\"")).count();
        println!(
            "{name}: {steps} runtime events -> {} domain events (TS {}) in {elapsed:?}; {} difference(s)",
            actual_events.len(),
            expected_events.len(),
            differences.len()
        );
        for difference in differences.iter().take(40) {
            println!("  {difference}");
        }
        if !differences.is_empty() {
            failed.push(name);
        }
    }
    assert!(failed.is_empty(), "the Rust ingestion differs from the TS one on: {failed:?}");
}
