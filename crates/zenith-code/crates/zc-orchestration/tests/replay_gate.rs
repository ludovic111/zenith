//! Gate 2 (replay): the Rust projector folds a real event log into the same command read model
//! as the TypeScript projector.
//!
//! ```sh
//! sqlite3 'file:<live state.sqlite>?mode=ro' ".backup '/tmp/copy/state.sqlite'"
//! node code/apps/server/scripts/orchestration-oracle.ts project /tmp/copy/state.sqlite > /tmp/copy/ts-model.json
//! ZC_REPLAY_DB=/tmp/copy/state.sqlite ZC_REPLAY_EXPECTED=/tmp/copy/ts-model.json \
//!   cargo test -p zc-orchestration --test replay_gate -- --ignored --nocapture
//! ```
//!
//! Only ever run it on a copy: the database is opened read-only, but the live one belongs to
//! the running server.

mod common;

use std::path::PathBuf;

use zc_db::repos::event_store;
use zc_db::Conn;
use zc_orchestration::engine::EventLogReads;
use zc_orchestration::event::decode_persisted_events;

#[test]
#[ignore = "needs ZC_REPLAY_DB and ZC_REPLAY_EXPECTED (see the module docs)"]
fn replaying_a_real_log_matches_the_typescript_projector() {
    let db_path = PathBuf::from(std::env::var("ZC_REPLAY_DB").expect("ZC_REPLAY_DB"));
    let expected_path = PathBuf::from(std::env::var("ZC_REPLAY_EXPECTED").expect("ZC_REPLAY_EXPECTED"));
    let conn = Conn::open_read_only(&db_path).expect("open the copy read-only");
    let rows = event_store::read_all(&conn).expect("read the event log");
    let events = decode_persisted_events(&rows, "replay").expect("decode every stored event");
    let started = std::time::Instant::now();
    let model = EventLogReads::fold(&events, "1970-01-01T00:00:00.000Z");
    let elapsed = started.elapsed();
    let actual = serde_json::to_value(&model).expect("encode the model");
    let expected: serde_json::Value = serde_json::from_slice(&std::fs::read(&expected_path).expect("read the TS model")).expect("parse the TS model");
    let differences = common::json_diff(&expected, &actual, 50);
    println!(
        "replayed {} events into {} projects / {} threads in {:?}; {} difference(s)",
        events.len(),
        model.projects.len(),
        model.threads.len(),
        elapsed,
        differences.len()
    );
    for difference in &differences {
        println!("  {difference}");
    }
    assert!(differences.is_empty(), "the Rust command read model differs from the TS one");
}
