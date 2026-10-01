//! Gate 3 (dispatch round trip): the same scripted commands, dispatched by the Rust engine on a
//! fresh database and by the TypeScript engine on another, persist the same events (payload and
//! metadata JSON, sequences, stream versions, actor kinds) and get the same answers.
//!
//! `fixtures/dispatch_gate.jsonl` is the script; `fixtures/dispatch_gate.expected.json` is what
//! the TS engine (`OrchestrationEngineLive` with the SQL projection pipeline) produced for it with
//! its clock pinned to [`NOW`]. Regenerate it with
//!
//! ```sh
//! cd code && node apps/server/scripts/orchestration-oracle.ts dispatch <empty dir> \
//!   --now 2026-08-01T12:00:00.000Z --out <file> \
//!   < ../crates/zenith-code/crates/zc-orchestration/tests/fixtures/dispatch_gate.jsonl
//! ```
//!
//! and point `ZC_DISPATCH_EXPECTED` at the output to compare against a fresh run instead of the
//! committed file. Event ids are random in TS, so both sides number them in order of appearance
//! before comparing.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};
use zc_contracts::{OrchestrationClientOrigin, OrchestrationCommand};
use zc_db::Db;
use zc_orchestration::engine::{EngineConfig, EventLogReads, NoBackgroundLiveness, OrchestrationEngine};
use zc_orchestration::pipeline::NoopProjectionPipeline;

const NOW: &str = "2026-08-01T12:00:00.000Z";
const SCRIPT: &str = include_str!("fixtures/dispatch_gate.jsonl");
const EXPECTED: &str = include_str!("fixtures/dispatch_gate.expected.json");

/// Every stored event row, with the columns the wire event lacks.
fn stored_events(path: &std::path::Path) -> Vec<Value> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut statement = conn
        .prepare(
            "SELECT sequence, event_id, event_type, aggregate_kind, stream_id, occurred_at, command_id,
                    causation_event_id, correlation_id, payload_json, metadata_json, stream_version, actor_kind
             FROM orchestration_events ORDER BY sequence ASC",
        )
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok(json!({
                "sequence": row.get::<_, i64>(0)?,
                "eventId": row.get::<_, String>(1)?,
                "type": row.get::<_, String>(2)?,
                "aggregateKind": row.get::<_, String>(3)?,
                "aggregateId": row.get::<_, String>(4)?,
                "occurredAt": row.get::<_, String>(5)?,
                "commandId": row.get::<_, Option<String>>(6)?,
                "causationEventId": row.get::<_, Option<String>>(7)?,
                "correlationId": row.get::<_, Option<String>>(8)?,
                "payload": serde_json::from_str::<Value>(&row.get::<_, String>(9)?).unwrap(),
                "metadata": serde_json::from_str::<Value>(&row.get::<_, String>(10)?).unwrap(),
                "streamVersion": row.get::<_, i64>(11)?,
                "actorKind": row.get::<_, String>(12)?,
            }))
        })
        .unwrap();
    rows.map(Result::unwrap).collect()
}

/// Numbers event ids in order of appearance (`#1`, `#2`, …), in `eventId` and
/// `causationEventId`.
fn normalize_ids(output: &mut Value) {
    let mut ids: HashMap<String, String> = HashMap::new();
    let mut rename = |value: &mut Value| {
        if let Some(id) = value.as_str() {
            let next = format!("#{}", ids.len() + 1);
            *value = Value::String(ids.entry(id.to_owned()).or_insert(next).clone());
        }
    };
    for event in output["events"].as_array_mut().unwrap() {
        rename(&mut event["eventId"]);
        rename(&mut event["causationEventId"]);
    }
}

async fn run_rust_engine() -> Value {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    let db = Db::open(&path).unwrap();
    let engine = OrchestrationEngine::start(EngineConfig {
        reads: Arc::new(EventLogReads::new(db.clone())),
        db,
        pipeline: Arc::new(NoopProjectionPipeline),
        liveness: Arc::new(NoBackgroundLiveness),
        env: Arc::new(common::TestEnv::at(NOW)),
    })
    .await
    .unwrap();
    let mut results = Vec::new();
    for line in SCRIPT.lines().filter(|line| !line.trim().is_empty()) {
        let input: Value = serde_json::from_str(line).unwrap();
        let command: OrchestrationCommand = common::decode(input["command"].clone());
        let origin: Option<OrchestrationClientOrigin> = input.get("origin").map(|origin| common::decode(origin.clone()));
        results.push(match engine.dispatch(command, origin).await {
            Ok(result) => json!({"sequence": result.sequence}),
            Err(error) => json!({"error": {"_tag": error.tag(), "message": error.to_string()}}),
        });
    }
    drop(engine);
    json!({"results": results, "events": stored_events(&path)})
}

#[tokio::test]
async fn the_rust_engine_persists_what_the_typescript_engine_persists() {
    let mut actual = run_rust_engine().await;
    let expected_text = match std::env::var("ZC_DISPATCH_EXPECTED") {
        Ok(path) => std::fs::read_to_string(path).unwrap(),
        Err(_) => EXPECTED.to_owned(),
    };
    let mut expected: Value = serde_json::from_str(&expected_text).unwrap();
    normalize_ids(&mut actual);
    normalize_ids(&mut expected);
    assert_eq!(
        expected["events"].as_array().unwrap().len(),
        actual["events"].as_array().unwrap().len(),
        "event count"
    );
    let differences = common::json_diff(&expected, &actual, 40);
    for difference in &differences {
        println!("  {difference}");
    }
    assert!(differences.is_empty(), "{} difference(s) with the TS engine", differences.len());
}
