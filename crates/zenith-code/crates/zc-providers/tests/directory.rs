//! Ports of `Layers/ProviderSessionDirectory.test.ts`.

use serde_json::{json, Value};
use zc_contracts::{ProviderInstanceId, RuntimeMode, ThreadId};
use zc_db::repos::provider_session_runtime::{self as repo, OnConflict, ProviderSessionRuntime};
use zc_db::Db;
use zc_providers::directory::{ProviderRuntimeBinding, ProviderSessionDirectory, RuntimeStatus};

fn binding(thread_id: &str, provider: &str, instance_id: &str) -> ProviderRuntimeBinding {
    ProviderRuntimeBinding::new(ThreadId::from(thread_id), provider.into(), ProviderInstanceId::from(instance_id))
}

fn imported_source(session_id: &str) -> Value {
    json!({
        "provider": "codex", "providerInstanceId": "codex", "providerSessionId": session_id, "filePath": "/tmp/provider-session.jsonl",
        "size": 100, "mtimeMs": 1000, "device": 1, "inode": 123, "birthtimeMs": 500
    })
}

async fn row(db: &Db, thread_id: &str) -> Option<ProviderSessionRuntime> {
    let id = thread_id.to_owned();
    db.call(move |conn| repo::get_by_thread_id(conn, &id)).await.unwrap()
}

async fn put(db: &Db, runtime: ProviderSessionRuntime) {
    db.call(move |conn| repo::upsert(conn, &runtime, OnConflict::Update)).await.unwrap();
}

#[allow(clippy::too_many_arguments)]
fn raw(
    thread_id: &str,
    provider: &str,
    instance: Option<&str>,
    mode: &str,
    status: &str,
    last_seen: &str,
    cursor: Option<Value>,
    payload: Option<Value>,
) -> ProviderSessionRuntime {
    ProviderSessionRuntime {
        thread_id: thread_id.into(),
        provider_name: provider.into(),
        provider_instance_id: instance.map(str::to_owned),
        adapter_key: provider.into(),
        runtime_mode: mode.into(),
        status: status.into(),
        last_seen_at: last_seen.into(),
        resume_cursor: cursor,
        runtime_payload: payload,
    }
}

#[tokio::test]
async fn upserts_and_reads_thread_bindings() {
    let db = Db::open_in_memory().unwrap();
    let directory = ProviderSessionDirectory::new(db.clone());
    directory.upsert(binding("thread-1", "codex", "codex"), OnConflict::Update).await.unwrap();
    assert_eq!(directory.get_provider(&"thread-1".into()).await.unwrap().as_str(), "codex");
    let resolved = directory.get_binding(&"thread-1".into()).await.unwrap().unwrap();
    assert_eq!(resolved.thread_id.as_str(), "thread-1");
    directory.upsert(binding("thread-2", "codex", "codex"), OnConflict::Update).await.unwrap();
    let runtime = row(&db, "thread-2").await.unwrap();
    assert_eq!(runtime.status, "running");
    assert_eq!(runtime.provider_name, "codex");
    assert_eq!(runtime.runtime_mode, "full-access");
    let ids: Vec<String> = directory.list_thread_ids().await.unwrap().into_iter().map(|id| id.to_string()).collect();
    assert!(ids.contains(&"thread-1".to_owned()) && ids.contains(&"thread-2".to_owned()));
    let missing = directory.get_provider(&"nope".into()).await.unwrap_err();
    assert_eq!(missing.tag(), "ProviderSessionDirectoryPersistenceError");
}

#[tokio::test]
async fn persists_runtime_fields_and_merges_payload_updates() {
    let db = Db::open_in_memory().unwrap();
    let directory = ProviderSessionDirectory::new(db.clone());
    let mut first = binding("thread-runtime", "codex", "codex");
    first.status = Some(RuntimeStatus::Starting);
    first.resume_cursor = Some(json!({"threadId": "provider-thread-runtime"}));
    first.runtime_payload = Some(json!({"cwd": "/tmp/project", "model": "gpt-5-codex"}));
    directory.upsert(first, OnConflict::Update).await.unwrap();
    let mut second = binding("thread-runtime", "codex", "codex");
    second.status = Some(RuntimeStatus::Running);
    second.runtime_payload = Some(json!({"activeTurnId": "turn-1"}));
    directory.upsert(second, OnConflict::Update).await.unwrap();
    let runtime = row(&db, "thread-runtime").await.unwrap();
    assert_eq!(runtime.status, "running");
    assert_eq!(runtime.resume_cursor, Some(json!({"threadId": "provider-thread-runtime"})));
    assert_eq!(
        runtime.runtime_payload,
        Some(json!({"cwd": "/tmp/project", "model": "gpt-5-codex", "activeTurnId": "turn-1"}))
    );
}

#[tokio::test]
async fn keeps_the_existing_binding_when_an_insert_conflicts() {
    let db = Db::open_in_memory().unwrap();
    let directory = ProviderSessionDirectory::new(db.clone());
    let mut active = binding("thread-insert-conflict", "codex", "codex");
    active.status = Some(RuntimeStatus::Running);
    active.resume_cursor = Some(json!({"threadId": "active-provider-thread"}));
    directory.upsert(active, OnConflict::Update).await.unwrap();
    let mut stale = binding("thread-insert-conflict", "codex", "codex");
    stale.status = Some(RuntimeStatus::Stopped);
    stale.resume_cursor = Some(json!({"threadId": "stale-provider-thread"}));
    directory.upsert(stale, OnConflict::Ignore).await.unwrap();
    let resolved = directory.get_binding(&"thread-insert-conflict".into()).await.unwrap().unwrap();
    assert_eq!(resolved.status, Some(RuntimeStatus::Running));
    assert_eq!(resolved.resume_cursor, Some(json!({"threadId": "active-provider-thread"})));
}

#[tokio::test]
async fn records_imported_transcripts_without_replacing_the_session() {
    let db = Db::open_in_memory().unwrap();
    let directory = ProviderSessionDirectory::new(db.clone());
    let thread_id = "import:codex:record-source";
    let mut current = binding(thread_id, "claudeAgent", "claude-current");
    current.status = Some(RuntimeStatus::Running);
    current.resume_cursor = Some(json!({"resume": "current-native-session"}));
    current.runtime_payload = Some(json!({"cwd": "/tmp/project", "activeTurnId": "active-turn"}));
    directory.upsert(current, OnConflict::Update).await.unwrap();
    let before = row(&db, thread_id).await.unwrap();
    let source = imported_source("record-source");
    directory.record_imported_transcript(&thread_id.into(), source.clone()).await.unwrap();
    let mut replacement = source.clone();
    replacement["size"] = json!(200);
    replacement["mtimeMs"] = json!(2000);
    directory.record_imported_transcript(&thread_id.into(), replacement.clone()).await.unwrap();
    let mut second_file = source.clone();
    second_file["filePath"] = json!("/tmp/provider-session-copy.jsonl");
    directory.record_imported_transcript(&thread_id.into(), second_file.clone()).await.unwrap();
    let after = row(&db, thread_id).await.unwrap();
    assert_eq!(after.resume_cursor, before.resume_cursor);
    assert_eq!(after.status, before.status);
    assert_eq!(
        after.runtime_payload,
        Some(json!({"cwd": "/tmp/project", "activeTurnId": "active-turn", "importedTranscripts": [replacement, second_file]}))
    );

    // No binding, no row.
    directory
        .record_imported_transcript(&"import:codex:missing".into(), imported_source("missing"))
        .await
        .unwrap();
    assert!(directory.get_binding(&"import:codex:missing".into()).await.unwrap().is_none());
}

#[tokio::test]
async fn imported_transcripts_survive_stale_runtime_writes_and_are_reserved() {
    let db = Db::open_in_memory().unwrap();
    let directory = ProviderSessionDirectory::new(db.clone());
    for (index, on_conflict) in [OnConflict::Update, OnConflict::Ignore].into_iter().enumerate() {
        let thread_id = format!("import:codex:reserved-{index}");
        let source = imported_source(&format!("reserved-{index}"));
        let mut with_sources = binding(&thread_id, "codex", "codex");
        with_sources.runtime_payload = Some(json!({"cwd": "/tmp/project", "importedTranscripts": [source.clone()]}));
        directory.upsert(with_sources, on_conflict).await.unwrap();
        assert_eq!(
            directory.get_binding(&thread_id.clone().into()).await.unwrap().unwrap().runtime_payload,
            Some(json!({"cwd": "/tmp/project"}))
        );
        directory.record_imported_transcript(&thread_id.clone().into(), source.clone()).await.unwrap();
        let mut cleared = binding(&thread_id, "codex", "codex");
        cleared.runtime_payload = Some(Value::Null);
        directory.upsert(cleared, OnConflict::Update).await.unwrap();
        assert_eq!(
            directory.get_binding(&thread_id.into()).await.unwrap().unwrap().runtime_payload,
            Some(json!({"importedTranscripts": [source]}))
        );
    }
}

#[tokio::test]
async fn lists_bindings_oldest_first_and_promotes_legacy_rows() {
    let db = Db::open_in_memory().unwrap();
    let directory = ProviderSessionDirectory::new(db.clone());
    put(
        &db,
        raw(
            "thread-runtime-newer",
            "codex",
            None,
            "full-access",
            "running",
            "2026-04-14T12:05:00.000Z",
            Some(json!({"opaque": "resume-newer"})),
            Some(json!({"cwd": "/tmp/newer"})),
        ),
    )
    .await;
    put(
        &db,
        raw(
            "thread-runtime-older",
            "claudeAgent",
            None,
            "approval-required",
            "starting",
            "2026-04-14T12:00:00.000Z",
            Some(json!({"opaque": "resume-older"})),
            Some(json!({"cwd": "/tmp/older"})),
        ),
    )
    .await;
    let bindings = directory.list_bindings(false).await.unwrap();
    assert_eq!(bindings.len(), 2);
    let older = &bindings[0];
    assert_eq!(older.last_seen_at, "2026-04-14T12:00:00.000Z");
    assert_eq!(older.binding.thread_id.as_str(), "thread-runtime-older");
    assert_eq!(older.binding.provider_instance_id, Some(ProviderInstanceId::from("claudeAgent")));
    assert_eq!(older.binding.adapter_key.as_deref(), Some("claudeAgent"));
    assert_eq!(older.binding.runtime_mode, Some(RuntimeMode::ApprovalRequired));
    assert_eq!(older.binding.status, Some(RuntimeStatus::Starting));
    assert_eq!(older.binding.resume_cursor, Some(json!({"opaque": "resume-older"})));
    assert_eq!(older.binding.runtime_payload, Some(json!({"cwd": "/tmp/older"})));
    assert_eq!(bindings[1].binding.provider_instance_id, Some(ProviderInstanceId::from("codex")));
}

#[tokio::test]
async fn lists_only_live_bindings_when_asked() {
    let db = Db::open_in_memory().unwrap();
    let directory = ProviderSessionDirectory::new(db.clone());
    for status in ["running", "starting", "error", "stopped"] {
        put(
            &db,
            raw(
                &format!("thread-exclude-stopped-{status}"),
                "codex",
                Some("codex"),
                "full-access",
                status,
                "2026-04-14T12:00:00.000Z",
                None,
                None,
            ),
        )
        .await;
    }
    let mut live: Vec<&str> = directory
        .list_bindings(true)
        .await
        .unwrap()
        .iter()
        .map(|b| b.binding.status.unwrap().as_str())
        .collect();
    live.sort();
    assert_eq!(live, vec!["error", "running", "starting"]);
    assert_eq!(directory.list_bindings(false).await.unwrap().len(), 4);
}

#[tokio::test]
async fn resets_the_adapter_key_when_the_provider_changes() {
    let db = Db::open_in_memory().unwrap();
    let directory = ProviderSessionDirectory::new(db.clone());
    put(
        &db,
        raw(
            "thread-provider-change",
            "claudeAgent",
            None,
            "full-access",
            "running",
            "2026-01-01T00:00:00.000Z",
            None,
            None,
        ),
    )
    .await;
    directory
        .upsert(binding("thread-provider-change", "codex", "codex"), OnConflict::Update)
        .await
        .unwrap();
    let runtime = row(&db, "thread-provider-change").await.unwrap();
    assert_eq!(runtime.provider_name, "codex");
    assert_eq!(runtime.adapter_key, "codex");

    // A provider change without an instance id is refused.
    let mut no_instance = binding("thread-provider-change", "cursor", "cursor");
    no_instance.provider_instance_id = None;
    let error = directory.upsert(no_instance, OnConflict::Update).await.unwrap_err();
    assert!(error.to_string().contains("providerInstanceId is required"));
}

#[tokio::test]
async fn rehydrates_bindings_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    {
        let db = Db::open(&path).unwrap();
        ProviderSessionDirectory::new(db)
            .upsert(binding("thread-restart", "codex", "codex"), OnConflict::Update)
            .await
            .unwrap();
    }
    let db = Db::open(&path).unwrap();
    let directory = ProviderSessionDirectory::new(db.clone());
    assert_eq!(directory.get_provider(&"thread-restart".into()).await.unwrap().as_str(), "codex");
    let legacy_tables: i64 = db
        .call(|conn| {
            conn.raw()
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'provider_sessions'",
                    [],
                    |row| row.get(0),
                )
                .map_err(|error| zc_db::DbError::sql("test", error))
        })
        .await
        .unwrap();
    assert_eq!(legacy_tables, 0);
}
