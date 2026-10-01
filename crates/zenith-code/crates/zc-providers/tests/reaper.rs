//! Ports of `Layers/ProviderSessionReaper.test.ts`.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use common::{Call, FakeAdapter};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use zc_contracts::{ProviderInstanceId, ThreadId};
use zc_db::repos::provider_session_runtime::{self as repo, OnConflict, ProviderSessionRuntime};
use zc_db::Db;
use zc_ports::adapter::ProviderAdapter;
use zc_providers::adapter_registry::StaticAdapterRegistry;
use zc_providers::hooks::{ThreadShellInfo, ThreadShells};
use zc_providers::reaper::{ProviderSessionReaper, ReaperOptions};
use zc_providers::{ProviderServiceImpl, ProviderServiceOptions, ProviderSessionDirectory};

const NOW_MS: i64 = 1_776_211_200_000; // 2026-04-15T00:00:00.000Z

struct Shells(HashMap<String, ThreadShellInfo>);

#[async_trait]
impl ThreadShells for Shells {
    async fn get_thread_shell(&self, thread_id: &ThreadId) -> Result<Option<ThreadShellInfo>, String> {
        Ok(self.0.get(thread_id.as_str()).cloned())
    }
}

fn shell(updated_at: &str, active_turn: Option<&str>, background: Option<serde_json::Value>) -> ThreadShellInfo {
    ThreadShellInfo {
        project_id: Some("project".into()),
        session_updated_at: Some(updated_at.into()),
        session_active_turn_id: active_turn.map(str::to_owned),
        background_liveness: background,
    }
}

async fn put(db: &Db, thread_id: &str, provider: &str, status: &str, last_seen: &str) {
    let runtime = ProviderSessionRuntime {
        thread_id: thread_id.into(),
        provider_name: provider.into(),
        provider_instance_id: None,
        adapter_key: provider.into(),
        runtime_mode: "full-access".into(),
        status: status.into(),
        last_seen_at: last_seen.into(),
        resume_cursor: Some(json!({"opaque": format!("resume-{thread_id}")})),
        runtime_payload: None,
    };
    db.call(move |conn| repo::upsert(conn, &runtime, OnConflict::Update)).await.unwrap();
}

async fn status(db: &Db, thread_id: &str) -> String {
    let id = thread_id.to_owned();
    db.call(move |conn| repo::get_by_thread_id(conn, &id)).await.unwrap().unwrap().status
}

struct Setup {
    db: Db,
    reaper: ProviderSessionReaper,
    codex: Arc<FakeAdapter>,
    claude: Arc<FakeAdapter>,
}

async fn setup(shells: Vec<(&str, ThreadShellInfo)>, instances: &[&str]) -> Setup {
    let db = Db::open_in_memory().unwrap();
    let directory = ProviderSessionDirectory::new(db.clone());
    let codex = FakeAdapter::new("codex");
    let claude = FakeAdapter::new("claudeAgent");
    let mut entries: Vec<(ProviderInstanceId, Arc<dyn ProviderAdapter>)> = Vec::new();
    if instances.contains(&"codex") {
        entries.push(("codex".into(), codex.clone()));
    }
    if instances.contains(&"claudeAgent") {
        entries.push(("claudeAgent".into(), claude.clone()));
    }
    let service = ProviderServiceImpl::start(
        Arc::new(StaticAdapterRegistry::new(entries)),
        directory.clone(),
        ProviderServiceOptions::new("/tmp/attachments"),
    )
    .await;
    let shells: Arc<dyn ThreadShells> = Arc::new(Shells(shells.into_iter().map(|(id, info)| (id.to_owned(), info)).collect()));
    let reaper = ProviderSessionReaper::new(
        service,
        directory,
        Some(shells),
        ReaperOptions {
            clock: Arc::new(|| NOW_MS),
            ..Default::default()
        },
    );
    Setup { db, reaper, codex, claude }
}

#[tokio::test]
async fn reaps_stale_sessions_without_active_turns() {
    let s = setup(vec![("thread-reaper-stale", shell("2026-01-01T00:00:00.000Z", None, None))], &["claudeAgent"]).await;
    put(&s.db, "thread-reaper-stale", "claudeAgent", "running", "2026-04-14T00:00:00.000Z").await;
    s.claude.insert_session(
        serde_json::from_value(json!({
            "provider": "claudeAgent", "providerInstanceId": "claudeAgent", "status": "ready", "runtimeMode": "full-access",
            "threadId": "thread-reaper-stale", "createdAt": common::NOW, "updatedAt": common::NOW
        }))
        .unwrap(),
    );
    assert_eq!(s.reaper.sweep().await.unwrap(), 1);
    assert!(s.claude.calls().contains(&Call::StopSession("thread-reaper-stale".into())));
    assert_eq!(status(&s.db, "thread-reaper-stale").await, "stopped");
}

#[tokio::test]
async fn skips_threads_with_an_active_turn_or_background_work() {
    let s = setup(
        vec![
            ("thread-active-turn", shell("2026-01-01T00:00:00.000Z", Some("turn-reaper-active"), None)),
            ("thread-background", shell("2026-01-01T00:00:00.000Z", None, Some(json!({"kind": "subagents"})))),
            ("thread-recent-settle", shell("2026-04-14T23:50:00.000Z", None, None)),
        ],
        &["claudeAgent"],
    )
    .await;
    for thread_id in ["thread-active-turn", "thread-background", "thread-recent-settle"] {
        put(&s.db, thread_id, "claudeAgent", "running", "2026-04-14T00:00:00.000Z").await;
    }
    // A binding seen recently is not even looked up.
    put(&s.db, "thread-fresh", "claudeAgent", "running", "2026-04-14T23:59:00.000Z").await;
    assert_eq!(s.reaper.sweep().await.unwrap(), 0);
    for thread_id in ["thread-active-turn", "thread-background", "thread-recent-settle", "thread-fresh"] {
        assert_eq!(status(&s.db, thread_id).await, "running", "{thread_id}");
    }
}

#[tokio::test]
async fn skips_rows_already_stopped() {
    let s = setup(vec![], &["codex"]).await;
    put(&s.db, "thread-stopped", "codex", "stopped", "2026-04-01T00:00:00.000Z").await;
    assert_eq!(s.reaper.sweep().await.unwrap(), 0);
    assert!(s.codex.calls().is_empty());
}

#[tokio::test]
async fn keeps_reaping_when_one_stop_fails() {
    // The first binding names an instance that no longer exists: its stop fails.
    let s = setup(vec![], &["codex"]).await;
    put(&s.db, "thread-reaper-stop-failure", "claudeAgent", "running", "2026-04-14T00:00:00.000Z").await;
    put(&s.db, "thread-reaper-stop-success", "codex", "running", "2026-04-14T00:01:00.000Z").await;
    assert_eq!(s.reaper.sweep().await.unwrap(), 1);
    assert_eq!(status(&s.db, "thread-reaper-stop-failure").await, "running");
    assert_eq!(status(&s.db, "thread-reaper-stop-success").await, "stopped");
}

#[tokio::test]
async fn the_background_loop_sweeps_immediately_and_stops_on_cancel() {
    let s = setup(vec![], &["codex"]).await;
    put(&s.db, "thread-loop", "codex", "running", "2026-04-14T00:00:00.000Z").await;
    let stop = CancellationToken::new();
    let task = s.reaper.start(stop.clone());
    let db = s.db.clone();
    for _ in 0..200 {
        if status(&db, "thread-loop").await == "stopped" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(status(&db, "thread-loop").await, "stopped");
    stop.cancel();
    task.await.unwrap();
}
