//! The repository tests of `persistence/**/*.test.ts`, ported (event store, projection
//! repositories, thread messages, thread activities, error correlation), plus coverage of the
//! repositories the TS suite only exercises through services (turns, sessions, approvals,
//! state, receipts, auth, provider runtime, files viewed).

use jiff::Timestamp;
use rusqlite::params;
use serde_json::{json, Value};
use zc_db::repos::*;
use zc_db::{migrations, Conn, Correlation, DbError};

fn fresh() -> Conn {
    let conn = Conn::open_in_memory().unwrap();
    migrations::run(&conn).unwrap();
    conn
}

fn ts(value: &str) -> Timestamp {
    value.parse().unwrap()
}

// ---------------------------------------------------------------- event store

fn message_event(thread_id: &str, id: &str) -> event_store::NewEvent {
    let now = "2026-01-01T00:00:00.000Z";
    event_store::NewEvent {
        event_id: id.into(),
        aggregate_kind: "thread".into(),
        aggregate_id: thread_id.into(),
        occurred_at: now.into(),
        command_id: None,
        causation_event_id: None,
        correlation_id: None,
        metadata: json!({}),
        event_type: "thread.message-sent".into(),
        payload: json!({
            "threadId": thread_id, "messageId": id, "role": "assistant", "text": id,
            "turnId": null, "streaming": false, "createdAt": now, "updatedAt": now,
        }),
    }
}

#[test]
fn event_store_stores_json_columns_as_strings_and_replays_cli_origin_events() {
    let conn = fresh();
    let now = "2026-01-01T00:00:00.000Z";
    let appended = event_store::append(
        &conn,
        &event_store::NewEvent {
            event_id: "evt-store-roundtrip".into(),
            aggregate_kind: "project".into(),
            aggregate_id: "project-roundtrip".into(),
            occurred_at: now.into(),
            command_id: Some("cmd-store-roundtrip".into()),
            causation_event_id: None,
            correlation_id: Some("cmd-store-roundtrip".into()),
            metadata: json!({ "adapterKey": "codex", "origin": { "surface": "cli" } }),
            event_type: "project.created".into(),
            payload: json!({
                "projectId": "project-roundtrip", "title": "Roundtrip Project",
                "workspaceRoot": "/tmp/project-roundtrip", "defaultModelSelection": null,
                "scripts": [], "createdAt": now, "updatedAt": now,
            }),
        },
    )
    .unwrap();
    assert_eq!(appended.sequence, 1);
    let (payload, metadata, actor, version): (String, String, String, i64) = conn
        .raw()
        .query_row(
            "SELECT payload_json, metadata_json, actor_kind, stream_version FROM orchestration_events WHERE event_id = ?1",
            params![appended.event_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert!(payload.starts_with("{\"projectId\":\"project-roundtrip\""));
    assert_eq!(metadata, r#"{"adapterKey":"codex","origin":{"surface":"cli"}}"#);
    // A client command id with adapterKey metadata is inferred as the provider.
    assert_eq!(actor, "provider");
    assert_eq!(version, 0);

    let replayed = event_store::read_from_sequence(&conn, 0, Some(10)).unwrap();
    assert_eq!(replayed.len(), 1);
    assert_eq!(replayed[0].event_type, "project.created");
    assert_eq!(replayed[0].metadata["adapterKey"], "codex");
    assert_eq!(replayed[0].metadata["origin"], json!({ "surface": "cli" }));
    assert_eq!(replayed[0], appended);
}

#[test]
fn event_store_infers_actor_kind_and_stream_versions() {
    let conn = fresh();
    let mut event = message_event("thread-a", "e1");
    event.command_id = Some("provider:rt-1:tag:uuid".into());
    event_store::append(&conn, &event).unwrap();
    let mut event = message_event("thread-a", "e2");
    event.command_id = Some("server:tag:uuid".into());
    event_store::append(&conn, &event).unwrap();
    let mut event = message_event("thread-a", "e3");
    event.command_id = Some("cmd-client".into());
    event_store::append(&conn, &event).unwrap();
    event_store::append(&conn, &message_event("thread-a", "e4")).unwrap();
    event_store::append(&conn, &message_event("thread-b", "e5")).unwrap();
    let rows: Vec<(String, i64)> = conn
        .raw()
        .prepare("SELECT actor_kind, stream_version FROM orchestration_events ORDER BY sequence")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            ("provider".into(), 0),
            ("server".into(), 1),
            ("client".into(), 2),
            ("server".into(), 3),
            ("server".into(), 0),
        ]
    );
    // The unique (aggregate_kind, stream_id, stream_version) index rejects a duplicate id.
    let error = event_store::append(&conn, &message_event("thread-a", "e1")).unwrap_err();
    assert_eq!(error.operation(), Some("OrchestrationEventStore.append:insert"));
    assert!(error.is_constraint());
}

#[test]
fn event_store_fails_with_decode_error_when_stored_json_is_invalid() {
    let conn = fresh();
    let sequence: i64 = conn
        .raw()
        .query_row(
            "INSERT INTO orchestration_events (event_id, aggregate_kind, stream_id, stream_version, event_type, occurred_at, command_id, causation_event_id, correlation_id, actor_kind, payload_json, metadata_json) VALUES ('evt-store-invalid-json', 'project', 'project-invalid-json', 0, 'project.created', '2026-01-01T00:00:00.000Z', 'cmd-store-invalid-json', NULL, NULL, 'server', '{', '{}') RETURNING sequence",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let error = event_store::read_from_sequence(&conn, 0, Some(10)).unwrap_err();
    assert!(error.is_decode());
    assert_eq!(error.operation(), Some("OrchestrationEventStore.readFromSequence:decodeRows"));
    let error = event_store::read_aggregate_range(
        &conn,
        &event_store::AggregateRange {
            aggregate_kind: "project".into(),
            aggregate_id: "project-invalid-json".into(),
            from_sequence_exclusive: 0,
            to_sequence_inclusive: sequence,
        },
        None,
    )
    .unwrap_err();
    assert!(error.is_decode());
    assert_eq!(error.operation(), Some("OrchestrationEventStore.readAggregateRange:decodeRows"));
}

#[test]
fn event_store_reads_one_aggregate_through_the_captured_head_across_pruned_gaps() {
    let conn = fresh();
    let thread = "shared-stream-id";
    let first = event_store::append(&conn, &message_event(thread, "scoped-first")).unwrap();
    let pruned = event_store::append(&conn, &message_event("pruned-thread", "pruned-event")).unwrap();
    let second = event_store::append(&conn, &message_event(thread, "scoped-second")).unwrap();
    conn.execute_batch(
        r#"
        INSERT INTO orchestration_events (
          event_id, aggregate_kind, stream_id, stream_version, event_type, occurred_at,
          actor_kind, payload_json, metadata_json
        ) VALUES (
          'same-id-project', 'project', 'shared-stream-id', 0, 'project.created',
          '2026-01-01T00:00:00.000Z', 'server', '{', '{'
        ), (
          'unrelated-invalid', 'thread', 'unrelated-invalid-thread', 0, 'thread.activity-appended',
          '2026-01-01T00:00:00.000Z', 'server', '{', '{'
        )
        "#,
    )
    .unwrap();
    let last = event_store::append(&conn, &message_event(thread, "scoped-last")).unwrap();
    conn.execute("DELETE FROM orchestration_events WHERE sequence = ?1", params![pruned.sequence])
        .unwrap();
    event_store::append(&conn, &message_event(thread, "after-captured-head")).unwrap();
    let events = event_store::read_aggregate_range(
        &conn,
        &event_store::AggregateRange {
            aggregate_kind: "thread".into(),
            aggregate_id: thread.into(),
            from_sequence_exclusive: first.sequence,
            to_sequence_inclusive: last.sequence,
        },
        Some(100),
    )
    .unwrap();
    assert_eq!(
        events.iter().map(|event| event.sequence).collect::<Vec<_>>(),
        vec![second.sequence, last.sequence]
    );
}

#[test]
fn event_store_bounds_replay_stats_and_counts_utf8_bytes() {
    let conn = fresh();
    let sequences: Vec<i64> = conn
        .raw()
        .prepare(
            r#"
        INSERT INTO orchestration_events (
          event_id, aggregate_kind, stream_id, stream_version, event_type, occurred_at,
          actor_kind, payload_json, metadata_json
        ) VALUES
          ('stats-1', 'thread', 'stats-thread', 0, 'thread.message-sent',
            '2026-01-01T00:00:00.000Z', 'provider', '{"output":"😀"}', '{}'),
          ('stats-unrelated', 'thread', 'another-thread', 0, 'thread.created',
            '2026-01-01T00:00:00.000Z', 'provider', printf('%.*c', 10000, 'x'), '{}'),
          ('stats-2', 'thread', 'stats-thread', 1, 'thread.activity-appended',
            '2026-01-01T00:00:00.000Z', 'provider', '{', '{}'),
          ('stats-other-kind', 'project', 'stats-thread', 0, 'project.deleted',
            '2026-01-01T00:00:00.000Z', 'provider', printf('%.*c', 20000, 'x'), '{}'),
          ('stats-3', 'thread', 'stats-thread', 2, 'thread.deleted',
            '2026-01-01T00:00:00.000Z', 'provider', '{"output":"é"}', '{}'),
          ('stats-4', 'thread', 'stats-thread', 3, 'thread.created',
            '2026-01-01T00:00:00.000Z', 'provider', printf('%.*c', 2000, 'x'), '{}')
        RETURNING sequence
        "#,
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let range = |to: i64| event_store::AggregateRange {
        aggregate_kind: "thread".into(),
        aggregate_id: "stats-thread".into(),
        from_sequence_exclusive: 0,
        to_sequence_inclusive: to,
    };
    let head = *sequences.last().unwrap();
    let stats = event_store::get_aggregate_replay_stats(&conn, &range(head), 2).unwrap();
    assert_eq!((stats.event_count, stats.payload_bytes, stats.has_create_event), (3, 33, false));
    let stats = event_store::get_aggregate_replay_stats(&conn, &range(head), 10).unwrap();
    assert_eq!((stats.event_count, stats.payload_bytes, stats.has_create_event), (4, 2033, true));
    let stats = event_store::get_aggregate_replay_stats(&conn, &range(sequences[2]), 10).unwrap();
    assert_eq!((stats.event_count, stats.payload_bytes, stats.has_create_event), (2, 18, false));
}

#[test]
fn event_store_keeps_later_pages_below_the_captured_head() {
    let conn = fresh();
    let persisted: Vec<_> = (0..502)
        .map(|index| event_store::append(&conn, &message_event("paged-thread", &format!("paged-{index}"))).unwrap())
        .collect();
    let head = persisted.last().unwrap().sequence;
    let mut pager = event_store::EventPager::aggregate_range(
        event_store::AggregateRange {
            aggregate_kind: "thread".into(),
            aggregate_id: "paged-thread".into(),
            from_sequence_exclusive: 0,
            to_sequence_inclusive: head,
        },
        Some(1_000),
    );
    let mut replayed = Vec::new();
    let mut appended_during_replay = false;
    while let Some(page) = pager.next_page(&conn).unwrap() {
        if !appended_during_replay {
            appended_during_replay = true;
            event_store::append(&conn, &message_event("paged-thread", "appended-during-replay")).unwrap();
        }
        replayed.extend(page.into_iter().map(|event| event.sequence));
    }
    assert_eq!(replayed, persisted.iter().map(|event| event.sequence).collect::<Vec<_>>());
    // TS normalizes 501.9 with Math.floor; callers pass the floored limit.
    for _ in 0..2 {
        let limited = event_store::read_from_sequence(&conn, persisted[0].sequence, Some(501)).unwrap();
        assert_eq!(
            limited.iter().map(|event| event.sequence).collect::<Vec<_>>(),
            persisted[1..].iter().map(|event| event.sequence).collect::<Vec<_>>()
        );
    }
    assert!(event_store::read_from_sequence(&conn, 0, Some(-1)).unwrap().is_empty());
}

#[test]
fn event_store_reads_all_in_pages_and_answers_has_event_after() {
    let conn = fresh();
    for index in 0..1_501 {
        event_store::append(&conn, &message_event("retention", &format!("retention-{index}"))).unwrap();
    }
    let mut pager = event_store::EventPager::all();
    let mut count = 0;
    let mut pages = 0;
    while let Some(page) = pager.next_page(&conn).unwrap() {
        assert!(page.len() <= 500);
        pages += 1;
        for event in page {
            count += 1;
            assert_eq!(event.sequence, count);
        }
    }
    assert_eq!(count, 1_501);
    assert_eq!(pages, 5); // 500 + 500 + 500 + 1 + an empty page that ends the read
    assert_eq!(event_store::latest_sequence(&conn).unwrap(), 1_501);
    assert!(event_store::has_event_after(&conn, "thread", "retention", None, 1_500).unwrap());
    assert!(!event_store::has_event_after(&conn, "thread", "retention", None, 1_501).unwrap());
    assert!(!event_store::has_event_after(&conn, "thread", "retention", Some("thread.deleted"), 0).unwrap());
    assert!(event_store::has_event_after(&conn, "thread", "retention", Some("thread.message-sent"), 0).unwrap());
}

#[tokio::test]
async fn event_store_streams_pages_through_the_actor() {
    use futures::StreamExt;
    let db = zc_db::Db::open_in_memory().unwrap();
    db.call(|conn| {
        for index in 0..1_200 {
            event_store::append(conn, &message_event("streamed", &format!("s-{index}")))?;
        }
        Ok(())
    })
    .await
    .unwrap();
    let events: Vec<_> = event_store::EventPager::from_sequence(100, Some(2_000)).into_stream(db.clone()).collect().await;
    assert_eq!(events.len(), 1_100);
    assert_eq!(events[0].as_ref().unwrap().sequence, 101);
    assert_eq!(events.last().unwrap().as_ref().unwrap().sequence, 1_200);
}

// ---------------------------------------------------------------- command receipts

#[test]
fn command_receipts_upsert_and_read() {
    let conn = fresh();
    assert_eq!(command_receipts::get_by_command_id(&conn, "cmd-1").unwrap(), None);
    let mut receipt = command_receipts::CommandReceipt {
        command_id: "cmd-1".into(),
        aggregate_kind: "thread".into(),
        aggregate_id: "thread-1".into(),
        accepted_at: "2026-01-01T00:00:00.000Z".into(),
        result_sequence: 4,
        status: "accepted".into(),
        error: None,
    };
    command_receipts::upsert(&conn, &receipt).unwrap();
    assert_eq!(command_receipts::get_by_command_id(&conn, "cmd-1").unwrap(), Some(receipt.clone()));
    receipt.status = "rejected".into();
    receipt.error = Some("Thread missing".into());
    command_receipts::upsert(&conn, &receipt).unwrap();
    assert_eq!(command_receipts::get_by_command_id(&conn, "cmd-1").unwrap(), Some(receipt));
}

// ---------------------------------------------------------------- proposed plans

fn plan(id: &str, thread: &str, turn: Option<&str>, implemented: Option<&str>, created: &str, updated: &str) -> proposed_plans::ProjectionThreadProposedPlan {
    proposed_plans::ProjectionThreadProposedPlan {
        plan_id: id.into(),
        thread_id: thread.into(),
        turn_id: turn.map(Into::into),
        plan_markdown: "# Plan".into(),
        implemented_at: implemented.map(Into::into),
        implementation_thread_id: None,
        created_at: created.into(),
        updated_at: updated.into(),
    }
}

#[test]
fn plans_select_the_latest_turn_plan_before_checking_implementation() {
    let conn = fresh();
    let thread = "thread-plan-status";
    let turn = "turn-plan-status-current";
    let first = plan(
        "plan-status-first",
        thread,
        Some(turn),
        None,
        "2026-03-24T00:00:01.000Z",
        "2026-03-24T00:00:01.000Z",
    );
    proposed_plans::upsert(&conn, &first).unwrap();
    proposed_plans::upsert(
        &conn,
        &plan(
            "plan-status-implemented",
            thread,
            Some(turn),
            Some("2026-03-24T00:00:02.000Z"),
            "2026-03-24T00:00:02.000Z",
            "2026-03-24T00:00:02.000Z",
        ),
    )
    .unwrap();
    proposed_plans::upsert(
        &conn,
        &plan(
            "plan-status-other-turn",
            thread,
            Some("turn-plan-status-old"),
            None,
            "2026-03-24T00:00:01.000Z",
            "2026-03-24T00:00:10.000Z",
        ),
    )
    .unwrap();
    assert!(!proposed_plans::has_actionable_by_thread_id(&conn, thread, Some(turn)).unwrap());
    assert!(proposed_plans::has_actionable_by_thread_id(&conn, thread, None).unwrap());
    proposed_plans::upsert(
        &conn,
        &proposed_plans::ProjectionThreadProposedPlan {
            updated_at: "2026-03-24T00:00:03.000Z".into(),
            ..first
        },
    )
    .unwrap();
    assert!(proposed_plans::has_actionable_by_thread_id(&conn, thread, Some(turn)).unwrap());
}

#[test]
fn plans_fall_back_within_the_thread_when_the_latest_turn_has_no_plan() {
    let conn = fresh();
    let thread = "thread-plan-fallback";
    let turn = "turn-plan-fallback-missing";
    assert!(!proposed_plans::has_actionable_by_thread_id(&conn, thread, Some(turn)).unwrap());
    assert!(!proposed_plans::has_actionable_by_thread_id(&conn, thread, None).unwrap());
    proposed_plans::upsert(
        &conn,
        &plan(
            "plan-fallback-without-turn",
            thread,
            None,
            Some("2026-03-24T00:00:01.000Z"),
            "2026-03-24T00:00:01.000Z",
            "2026-03-24T00:00:01.000Z",
        ),
    )
    .unwrap();
    proposed_plans::upsert(
        &conn,
        &plan(
            "plan-fallback-with-turn",
            thread,
            Some("turn-plan-fallback-old"),
            None,
            "2026-03-24T00:00:01.000Z",
            "2026-03-24T00:00:02.000Z",
        ),
    )
    .unwrap();
    proposed_plans::upsert(
        &conn,
        &plan(
            "plan-fallback-other-thread",
            "thread-plan-fallback-other",
            Some(turn),
            Some("2026-03-24T00:00:01.000Z"),
            "2026-03-24T00:00:01.000Z",
            "2026-03-24T00:00:03.000Z",
        ),
    )
    .unwrap();
    assert!(proposed_plans::has_actionable_by_thread_id(&conn, thread, Some(turn)).unwrap());
    assert!(proposed_plans::has_actionable_by_thread_id(&conn, thread, None).unwrap());
}

#[test]
fn plans_preserve_locale_ordering_and_stable_ties() {
    let conn = fresh();
    let stamp = "2026-03-24T00:00:00.000Z";
    // (planId, implementedAt, createdAt, updatedAt)
    type PlanRow<'a> = (&'a str, Option<&'a str>, &'a str, &'a str);
    // (name, expected, rows)
    let cases: Vec<(&str, bool, Vec<PlanRow>)> = vec![
        // "plan-A".localeCompare("plan-a") > 0 in ICU.
        (
            "mixed-case-ids",
            true,
            vec![("plan-a", Some(stamp), stamp, stamp), ("plan-A", None, stamp, stamp)],
        ),
        (
            "equivalent-ids",
            true,
            vec![
                ("plan-\u{e9}", Some(stamp), stamp, stamp),
                ("plan-e\u{301}", None, "2026-03-24T00:00:01.000Z", stamp),
            ],
        ),
        // "…+00:00".localeCompare("…-01:00") > 0 in ICU.
        (
            "timestamp-formats",
            true,
            vec![
                ("plan-minus", Some(stamp), stamp, "2026-03-24T00:00:00-01:00"),
                ("plan-plus", None, stamp, "2026-03-24T00:00:00+00:00"),
            ],
        ),
    ];
    for (name, expected, rows) in cases {
        let thread = format!("thread-plan-order-{name}");
        let turn = format!("turn-plan-order-{name}");
        for (id, implemented, created, updated) in rows {
            proposed_plans::upsert(&conn, &plan(&format!("{name}-{id}"), &thread, Some(&turn), implemented, created, updated)).unwrap();
        }
        assert_eq!(
            proposed_plans::has_actionable_by_thread_id(&conn, &thread, Some(&turn)).unwrap(),
            expected,
            "{name}"
        );
        assert_eq!(proposed_plans::has_actionable_by_thread_id(&conn, &thread, None).unwrap(), expected, "{name}");
    }
}

#[test]
fn plans_read_only_the_requested_plan_in_its_thread() {
    let conn = fresh();
    let mut target = plan(
        "plan-query-target",
        "plan-query-thread",
        None,
        Some("2026-03-01T00:01:00.000Z"),
        "2026-03-01T00:00:00.000Z",
        "2026-03-01T00:01:00.000Z",
    );
    target.plan_markdown = "  Keep this plan  ".into();
    target.implementation_thread_id = Some("implementation-thread".into());
    proposed_plans::upsert(&conn, &target).unwrap();
    // An unrelated old row that does not decode must not be loaded by the exact lookup.
    conn.execute_batch("INSERT INTO projection_thread_proposed_plans (plan_id, thread_id, turn_id, plan_markdown, implemented_at, implementation_thread_id, created_at, updated_at) VALUES ('unrelated-plan', 'plan-query-thread', NULL, '', NULL, NULL, '2026-03-01T00:00:00.000Z', '2026-03-01T00:00:00.000Z')").unwrap();
    let found = proposed_plans::get_by_plan_id(&conn, "plan-query-thread", "plan-query-target")
        .unwrap()
        .unwrap();
    assert_eq!(found.plan_markdown, "Keep this plan");
    assert_eq!(found.implemented_at.as_deref(), Some("2026-03-01T00:01:00.000Z"));
    assert!(proposed_plans::get_by_plan_id(&conn, "another-thread", "plan-query-target").unwrap().is_none());
    // The whole-thread list does decode it, and fails like the TS repository.
    assert!(proposed_plans::list_by_thread_id(&conn, "plan-query-thread").is_err());
    proposed_plans::delete_by_thread_id(&conn, "plan-query-thread").unwrap();
    assert!(proposed_plans::list_by_thread_id(&conn, "plan-query-thread").unwrap().is_empty());
}

// ---------------------------------------------------------------- projects and threads

#[test]
fn projects_store_model_selection_json_and_round_trip() {
    let conn = fresh();
    let project = projects::ProjectionProject {
        project_id: "project-null-options".into(),
        title: "Null options project".into(),
        workspace_root: "/tmp/project-null-options".into(),
        default_model_selection: Some(json!({ "instanceId": "codex", "model": "gpt-5.4" })),
        default_thread_env_mode: None,
        auto_pull: false,
        favicon_path: None,
        project_icon: None,
        scripts: json!([]),
        created_at: "2026-03-24T00:00:00.000Z".into(),
        updated_at: "2026-03-24T00:00:00.000Z".into(),
        deleted_at: None,
    };
    projects::upsert(&conn, &project).unwrap();
    let stored: (Option<String>, Option<String>, i64) = conn
        .raw()
        .query_row(
            "SELECT default_model_selection_json, project_icon_json, auto_pull FROM projection_projects WHERE project_id = 'project-null-options'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(stored, (Some(r#"{"instanceId":"codex","model":"gpt-5.4"}"#.into()), None, 0));
    assert_eq!(projects::get_by_id(&conn, "project-null-options").unwrap(), Some(project.clone()));

    let updated = projects::ProjectionProject {
        default_model_selection: None,
        auto_pull: true,
        project_icon: Some(json!({ "kind": "emoji", "value": "🚀" })),
        favicon_path: Some("/tmp/favicon.png".into()),
        ..project
    };
    projects::upsert(&conn, &updated).unwrap();
    assert_eq!(projects::get_by_id(&conn, "project-null-options").unwrap(), Some(updated));
    assert_eq!(projects::get_by_id(&conn, "missing").unwrap(), None);
}

fn thread_row(id: &str) -> threads::ProjectionThread {
    threads::ProjectionThread {
        thread_id: id.into(),
        project_id: "project-1".into(),
        title: "Thread".into(),
        title_state: None,
        model_selection: json!({ "instanceId": "codex", "model": "gpt-5.4" }),
        runtime_mode: "full-access".into(),
        interaction_mode: "default".into(),
        branch: None,
        worktree_path: None,
        linked_pull_request: None,
        branch_pull_request: None,
        latest_turn_id: None,
        created_at: "2026-03-24T00:00:00.000Z".into(),
        updated_at: "2026-03-24T00:00:00.000Z".into(),
        archived_at: None,
        settled_override: None,
        settled_at: None,
        unsettled_at: None,
        snoozed_until: None,
        snoozed_at: None,
        pinned_at: None,
        pin_order_key: None,
        active_order_key: None,
        auto_settle_disabled_at: None,
        title_regeneration_request_id: None,
        title_regeneration_started_at: None,
        latest_user_message_at: None,
        pending_approval_count: 0,
        pending_user_input_count: 0,
        has_actionable_proposed_plan: 0,
        deleted_at: None,
    }
}

#[test]
fn threads_store_model_selection_json() {
    let conn = fresh();
    let mut row = thread_row("thread-null-options");
    row.model_selection = json!({ "instanceId": "claudeAgent", "model": "claude-opus-4-6" });
    threads::upsert(&conn, &row).unwrap();
    let stored: String = conn
        .raw()
        .query_row(
            "SELECT model_selection_json FROM projection_threads WHERE thread_id = 'thread-null-options'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, r#"{"instanceId":"claudeAgent","model":"claude-opus-4-6"}"#);
    assert_eq!(threads::get_by_id(&conn, "thread-null-options").unwrap(), Some(row));
}

#[test]
fn threads_round_trip_settlement_values() {
    let conn = fresh();
    let mut row = thread_row("thread-settled");
    row.updated_at = "2026-03-25T00:00:00.000Z".into();
    row.settled_override = Some("settled".into());
    row.settled_at = Some("2026-03-25T00:00:00.000Z".into());
    row.snoozed_until = Some("2026-03-26T09:00:00.000Z".into());
    row.snoozed_at = Some("2026-03-25T00:00:00.000Z".into());
    row.pinned_at = Some("2026-03-25T00:00:00.000Z".into());
    row.pin_order_key = Some("a0".into());
    row.active_order_key = Some("b1".into());
    row.auto_settle_disabled_at = Some("2026-03-25T00:00:00.000Z".into());
    row.title_regeneration_request_id = Some("cmd-title".into());
    row.title_regeneration_started_at = Some("2026-03-25T00:00:00.000Z".into());
    row.title_state = Some(json!({ "source": "generated" }));
    threads::upsert(&conn, &row).unwrap();
    let persisted = threads::get_by_id(&conn, "thread-settled").unwrap().unwrap();
    assert_eq!(persisted, row);

    let flipped = threads::ProjectionThread {
        settled_override: Some("active".into()),
        settled_at: None,
        unsettled_at: Some("2026-03-26T00:00:00.000Z".into()),
        snoozed_until: None,
        snoozed_at: None,
        pinned_at: None,
        ..persisted
    };
    threads::upsert(&conn, &flipped).unwrap();
    assert_eq!(threads::get_by_id(&conn, "thread-settled").unwrap(), Some(flipped));
}

#[test]
fn threads_round_trip_manual_and_branch_pull_requests() {
    let conn = fresh();
    let linked =
        json!({ "projectId": "project-linked-pr", "repository": "pingdotgg/t3code", "number": 42, "url": "https://github.com/pingdotgg/t3code/pull/42" });
    let branch =
        json!({ "projectId": "project-linked-pr", "repository": "pingdotgg/t3code", "number": 43, "url": "https://github.com/pingdotgg/t3code/pull/43" });
    let mut row = thread_row("thread-linked-pr");
    row.linked_pull_request = Some(linked.clone());
    row.branch_pull_request = Some(branch.clone());
    threads::upsert(&conn, &row).unwrap();
    let persisted = threads::get_by_id(&conn, "thread-linked-pr").unwrap().unwrap();
    assert_eq!(persisted.linked_pull_request, Some(linked.clone()));
    assert_eq!(persisted.branch_pull_request, Some(branch.clone()));
    threads::upsert(
        &conn,
        &threads::ProjectionThread {
            linked_pull_request: None,
            ..persisted.clone()
        },
    )
    .unwrap();
    let cleared = threads::get_by_id(&conn, "thread-linked-pr").unwrap().unwrap();
    assert_eq!(cleared.linked_pull_request, None);
    assert_eq!(cleared.branch_pull_request, Some(branch));
    threads::upsert(
        &conn,
        &threads::ProjectionThread {
            branch_pull_request: None,
            ..persisted
        },
    )
    .unwrap();
    let cleared = threads::get_by_id(&conn, "thread-linked-pr").unwrap().unwrap();
    assert_eq!(cleared.branch_pull_request, None);
    assert_eq!(cleared.linked_pull_request, Some(linked));
}

// ---------------------------------------------------------------- thread pull requests

fn pr_link(thread: &str, number: i64, source: &str, linked_at: &str) -> thread_pull_requests::ProjectionThreadPullRequest {
    thread_pull_requests::ProjectionThreadPullRequest {
        thread_id: thread.into(),
        host: "github.com".into(),
        repository: "pingdotgg/t3code".into(),
        number,
        url: format!("https://github.com/pingdotgg/t3code/pull/{number}"),
        source: source.into(),
        linked_at: linked_at.into(),
        snapshot: None,
        stack: None,
    }
}

#[test]
fn pull_requests_use_one_azure_identity_for_writes_lookups_and_deletion() {
    let conn = fresh();
    let row = thread_pull_requests::ProjectionThreadPullRequest {
        thread_id: "azure-alias-link".into(),
        host: "org.visualstudio.com".into(),
        repository: "project/_git/web".into(),
        number: 7,
        url: "https://org.visualstudio.com/project/_git/web/pullrequest/7".into(),
        source: "manual".into(),
        linked_at: "2026-09-09T00:00:00.000Z".into(),
        snapshot: None,
        stack: None,
    };
    thread_pull_requests::upsert(&conn, &row).unwrap();
    thread_pull_requests::upsert(
        &conn,
        &thread_pull_requests::ProjectionThreadPullRequest {
            host: "dev.azure.com".into(),
            repository: "org/project/_git/web".into(),
            ..row.clone()
        },
    )
    .unwrap();
    let found = thread_pull_requests::list_by_pull_request(&conn, "ssh.dev.azure.com", "v3/org/project/web", 7).unwrap();
    assert_eq!(
        found,
        vec![thread_pull_requests::ProjectionThreadPullRequest {
            host: "dev.azure.com".into(),
            repository: "org/project/_git/web".into(),
            ..row
        }]
    );
    assert!(thread_pull_requests::list_by_pull_request(&conn, "dev.azure.com", "other/project/_git/web", 7)
        .unwrap()
        .is_empty());
    thread_pull_requests::delete(&conn, "azure-alias-link", "vs-ssh.visualstudio.com", "v3/org/project/web", 7).unwrap();
    assert!(thread_pull_requests::list_by_thread_id(&conn, "azure-alias-link").unwrap().is_empty());
}

#[test]
fn pull_requests_round_trip_snapshot_and_stack_columns() {
    let conn = fresh();
    let thread = "thread-pr-links";
    let other = "thread-pr-links-other";
    let unsynced = pr_link(thread, 42, "manual", "2026-03-24T00:00:00.000Z");
    let mut synced = pr_link(thread, 7, "stack", "2026-03-23T00:00:00.000Z");
    synced.snapshot = Some(
        json!({ "state": "open", "title": "Add links", "headBranch": "feat/links", "baseBranch": "main", "isDraft": false, "updatedAt": "2026-03-23T01:00:00.000Z", "syncedAt": "2026-03-23T02:00:00.000Z" }),
    );
    synced.stack = Some(
        json!({ "kind": "native", "id": "stack-1", "number": 1, "url": "https://github.com/pingdotgg/t3code/stack/1", "base": "main", "layers": [{ "number": 7, "headBranch": "feat/links", "state": "open" }, { "number": 42, "headBranch": "feat/links-ui", "state": "open" }] }),
    );
    let shared = thread_pull_requests::ProjectionThreadPullRequest {
        thread_id: other.into(),
        source: "agent".into(),
        linked_at: "2026-03-25T00:00:00.000Z".into(),
        ..unsynced.clone()
    };
    for row in [&unsynced, &synced, &shared] {
        thread_pull_requests::upsert(&conn, row).unwrap();
    }
    let raw: Vec<(i64, Option<String>, Option<String>)> = conn
        .raw()
        .prepare("SELECT number, snapshot_json, stack_json FROM projection_thread_pull_requests WHERE thread_id = ?1 ORDER BY number ASC")
        .unwrap()
        .query_map(params![thread], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(raw[0].0, 7);
    assert_eq!(
        serde_json::from_str::<Value>(raw[0].1.as_deref().unwrap()).unwrap(),
        synced.snapshot.clone().unwrap()
    );
    assert_eq!(
        serde_json::from_str::<Value>(raw[0].2.as_deref().unwrap()).unwrap(),
        synced.stack.clone().unwrap()
    );
    assert_eq!((raw[1].1.clone(), raw[1].2.clone()), (None, None));

    assert_eq!(
        thread_pull_requests::list_by_thread_id(&conn, thread).unwrap(),
        vec![synced.clone(), unsynced.clone()]
    );
    assert_eq!(
        thread_pull_requests::list_by_pull_request(&conn, "github.com", "pingdotgg/t3code", 42).unwrap(),
        vec![unsynced.clone(), shared.clone()]
    );

    let resynced = thread_pull_requests::ProjectionThreadPullRequest {
        snapshot: synced.snapshot.clone(),
        stack: None,
        ..unsynced
    };
    thread_pull_requests::upsert(&conn, &resynced).unwrap();
    assert_eq!(thread_pull_requests::list_by_thread_id(&conn, thread).unwrap(), vec![synced, resynced.clone()]);
    thread_pull_requests::delete(&conn, thread, "github.com", "pingdotgg/t3code", 7).unwrap();
    assert_eq!(thread_pull_requests::list_by_thread_id(&conn, thread).unwrap(), vec![resynced]);
    thread_pull_requests::delete_by_thread_id_and_source(&conn, thread, "manual").unwrap();
    assert!(thread_pull_requests::list_by_thread_id(&conn, thread).unwrap().is_empty());
    assert_eq!(thread_pull_requests::list_by_thread_id(&conn, other).unwrap(), vec![shared]);
    thread_pull_requests::delete_by_thread_id(&conn, other).unwrap();
    assert!(thread_pull_requests::list_by_thread_id(&conn, other).unwrap().is_empty());
}

// ---------------------------------------------------------------- thread messages

fn message(id: &str, thread: &str, role: &str, created: &str) -> thread_messages::ProjectionThreadMessage {
    thread_messages::ProjectionThreadMessage {
        message_id: id.into(),
        thread_id: thread.into(),
        turn_id: None,
        role: role.into(),
        text: "Message body".into(),
        attachments: None,
        context: None,
        is_streaming: false,
        created_at: created.into(),
        updated_at: created.into(),
    }
}

#[test]
fn messages_find_the_latest_live_user_message_time() {
    let conn = fresh();
    let thread = "thread-latest-user-message";
    assert_eq!(thread_messages::get_latest_user_message_at(&conn, thread).unwrap(), None);
    thread_messages::upsert(
        &conn,
        &message("import:codex:latest-user-message:000000", thread, "user", "2026-02-28T19:05:06.000Z"),
    )
    .unwrap();
    assert_eq!(thread_messages::get_latest_user_message_at(&conn, thread).unwrap(), None);
    for (index, (role, created)) in [
        ("user", "2026-02-28T19:05:02.000Z"),
        ("user", "2026-02-28T19:05:01.000Z"),
        ("assistant", "2026-02-28T19:05:03.000Z"),
        ("system", "2026-02-28T19:05:04.000Z"),
    ]
    .into_iter()
    .enumerate()
    {
        thread_messages::upsert(&conn, &message(&format!("latest-user-message-{index}"), thread, role, created)).unwrap();
    }
    thread_messages::upsert(
        &conn,
        &message(
            "latest-user-message-other-thread",
            "thread-latest-user-message-other",
            "user",
            "2026-02-28T19:05:05.000Z",
        ),
    )
    .unwrap();
    assert_eq!(
        thread_messages::get_latest_user_message_at(&conn, thread).unwrap().as_deref(),
        Some("2026-02-28T19:05:02.000Z")
    );
    thread_messages::delete_by_thread_id(&conn, thread).unwrap();
    assert_eq!(thread_messages::get_latest_user_message_at(&conn, thread).unwrap(), None);
}

#[test]
fn messages_keep_context_across_updates_without_context() {
    let conn = fresh();
    let context = json!({ "version": 1, "records": [{ "version": 1, "contextId": "ctx_1", "kind": "terminal", "label": "Terminal 1 line 4", "terminalId": "default", "terminalLabel": "Terminal 1", "lineStart": 4, "lineEnd": 4, "text": "boom" }] });
    let mut row = message("message-context", "thread-context", "user", "2026-02-28T19:05:00.000Z");
    row.context = Some(context.clone());
    thread_messages::upsert(&conn, &row).unwrap();
    row.context = None;
    row.updated_at = "2026-02-28T19:05:01.000Z".into();
    thread_messages::upsert(&conn, &row).unwrap();
    let rows = thread_messages::list_by_thread_id(&conn, "thread-context").unwrap();
    assert_eq!(rows[0].context, Some(context));
}

#[test]
fn messages_append_streaming_text_in_sql_and_apply_attachment_updates() {
    let conn = fresh();
    let attachments = json!([{ "type": "image", "id": "thread-streaming-append-att-1", "name": "example.png", "mimeType": "image/png", "sizeBytes": 5 }]);
    let created = "2026-02-28T19:05:00.000Z";
    let delta = |text: &str, attachments: Option<Value>, at: &str| thread_messages::AppendStreamingMessage {
        message_id: "message-streaming-append".into(),
        thread_id: "thread-streaming-append".into(),
        turn_id: None,
        role: "assistant".into(),
        text: text.into(),
        attachments,
        context: None,
        created_at: at.into(),
        updated_at: at.into(),
    };
    thread_messages::append_streaming(&conn, &delta("hello", Some(attachments.clone()), created)).unwrap();
    thread_messages::append_streaming(&conn, &delta(" world", None, "2026-02-28T19:05:01.000Z")).unwrap();
    let row = thread_messages::get_by_message_id(&conn, "message-streaming-append").unwrap().unwrap();
    assert_eq!(row.attachments, Some(attachments));
    thread_messages::append_streaming(&conn, &delta("", Some(json!([])), "2026-02-28T19:05:02.000Z")).unwrap();
    let row = thread_messages::get_by_message_id(&conn, "message-streaming-append").unwrap().unwrap();
    assert_eq!(row.text, "hello world");
    assert_eq!(row.attachments, Some(json!([])));
    assert_eq!(row.created_at, created);
    assert_eq!(row.updated_at, "2026-02-28T19:05:02.000Z");
    assert!(row.is_streaming);
}

#[test]
fn messages_preserve_attachments_when_upsert_omits_them_and_clear_with_empty() {
    let conn = fresh();
    let attachments = json!([{ "type": "image", "id": "att-1", "name": "example.png", "mimeType": "image/png", "sizeBytes": 5 }]);
    let mut row = message("message-preserve", "thread-preserve", "user", "2026-02-28T19:00:00.000Z");
    row.text = "initial".into();
    row.attachments = Some(attachments.clone());
    thread_messages::upsert(&conn, &row).unwrap();
    row.text = "updated".into();
    row.attachments = None;
    thread_messages::upsert(&conn, &row).unwrap();
    let rows = thread_messages::list_by_thread_id(&conn, "thread-preserve").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].text, "updated");
    assert_eq!(rows[0].attachments, Some(attachments));
    row.text = "cleared".into();
    row.attachments = Some(json!([]));
    thread_messages::upsert(&conn, &row).unwrap();
    let rows = thread_messages::list_by_thread_id(&conn, "thread-preserve").unwrap();
    assert_eq!(rows[0].attachments, Some(json!([])));
    // A message stored without attachments reads back without the key.
    thread_messages::upsert(&conn, &message("bare", "thread-preserve", "user", "2026-02-28T19:00:01.000Z")).unwrap();
    let bare = thread_messages::get_by_message_id(&conn, "bare").unwrap().unwrap();
    assert_eq!(bare.attachments, None);
    assert!(serde_json::to_value(&bare).unwrap().get("attachments").is_none());
}

#[test]
fn messages_check_assistant_turn_state_without_text() {
    let conn = fresh();
    let mut row = message(
        "message-assistant-turn-state",
        "thread-assistant-turn-state",
        "assistant",
        "2026-03-01T00:00:00.000Z",
    );
    row.turn_id = Some("turn-assistant-state".into());
    thread_messages::upsert(&conn, &row).unwrap();
    assert!(thread_messages::has_assistant_message_for_turn(&conn, "thread-assistant-turn-state", "turn-assistant-state", false).unwrap());
    assert!(!thread_messages::has_assistant_message_for_turn(&conn, "thread-assistant-turn-state", "turn-assistant-state", true).unwrap());
    assert!(!thread_messages::has_assistant_message_for_turn(&conn, "thread-assistant-turn-state", "turn-assistant-state-missing", false).unwrap());
}

// ---------------------------------------------------------------- thread activities

#[test]
fn activities_read_only_the_latest_matching_task_activity() {
    let conn = fresh();
    let thread = "thread-latest-task-activity";
    conn.execute(
        r#"
        INSERT INTO projection_thread_activities (activity_id, thread_id, turn_id, tone, kind, summary, payload_json, sequence, created_at) VALUES
          ('activity-task-unrelated-tool', ?1, NULL, 'tool', 'tool.completed', 'large tool output', 'not-json', 1, '2026-03-01T00:00:00.000Z'),
          ('activity-task-started', ?1, NULL, 'info', 'task.started', 'started', '{"taskId":"task-1","title":"Initial title"}', 2, '2026-03-01T00:00:01.000Z'),
          ('activity-task-progress', ?1, NULL, 'info', 'task.progress', 'progress', '{"taskId":"task-1","title":"Updated title"}', 3, '2026-03-01T00:00:02.000Z'),
          ('activity-task-other', ?1, NULL, 'info', 'task.progress', 'other', '{"taskId":"task-2","title":"Other title"}', 4, '2026-03-01T00:00:03.000Z')
        "#,
        params![thread],
    )
    .unwrap();
    let activity = |id: &str, payload: Value, sequence: i64, created: &str| thread_activities::ProjectionThreadActivity {
        activity_id: id.into(),
        thread_id: thread.into(),
        turn_id: None,
        tone: "info".into(),
        kind: "task.progress".into(),
        summary: "Still running".into(),
        payload,
        sequence: Some(sequence),
        created_at: created.into(),
    };
    thread_activities::upsert(
        &conn,
        &activity("activity-task-untitled", json!({ "taskId": "task-1" }), 5, "2026-03-01T00:00:04.000Z"),
    )
    .unwrap();
    thread_activities::upsert(
        &conn,
        &activity(
            "activity-task-blank-title",
            json!({ "taskId": "task-1", "title": " \t\n\u{a0}" }),
            6,
            "2026-03-01T00:00:05.000Z",
        ),
    )
    .unwrap();

    let recent = thread_activities::list_by_thread_id(&conn, thread, Some(&["task.progress".to_string()]), Some(2)).unwrap();
    assert_eq!(
        recent.iter().map(|a| a.activity_id.as_str()).collect::<Vec<_>>(),
        vec!["activity-task-untitled", "activity-task-blank-title"]
    );
    let latest = thread_activities::get_latest_task_activity(&conn, thread, "task-1").unwrap().unwrap();
    assert_eq!(latest.activity_id, "activity-task-progress");
    assert_eq!(latest.payload, json!({ "taskId": "task-1", "title": "Updated title" }));
    assert!(thread_activities::get_latest_task_activity(&conn, thread, "missing").unwrap().is_none());
    // An empty kind filter matches nothing (`sql.in` with no values is `1=0`).
    assert!(thread_activities::list_by_thread_id(&conn, thread, Some(&[]), None).unwrap().is_empty());
    // The whole list decodes the invalid row and fails with the decode operation.
    let error = thread_activities::list_by_thread_id(&conn, thread, None, None).unwrap_err();
    assert_eq!(error.operation(), Some("ProjectionThreadActivityRepository.listByThreadId:decodeRows"));
}

#[test]
fn activities_list_user_input_lifecycle_in_replay_order() {
    let conn = fresh();
    let mut activity = thread_activities::ProjectionThreadActivity {
        activity_id: "a-1".into(),
        thread_id: "t".into(),
        turn_id: None,
        tone: "info".into(),
        kind: "user-input.requested".into(),
        summary: "asked".into(),
        payload: json!({ "requestId": "r" }),
        sequence: Some(2),
        created_at: "2026-03-01T00:00:00.000Z".into(),
    };
    thread_activities::upsert(&conn, &activity).unwrap();
    activity.activity_id = "a-0".into();
    activity.sequence = None;
    activity.kind = "user-input.resolved".into();
    thread_activities::upsert(&conn, &activity).unwrap();
    activity.activity_id = "a-2".into();
    activity.kind = "tool.completed".into();
    activity.sequence = Some(1);
    thread_activities::upsert(&conn, &activity).unwrap();
    let lifecycle = thread_activities::list_user_input_lifecycle_by_thread_id(&conn, "t").unwrap();
    assert_eq!(lifecycle.iter().map(|a| a.activity_id.as_str()).collect::<Vec<_>>(), vec!["a-0", "a-1"]);
    assert_eq!(lifecycle[0].sequence, None);
    thread_activities::delete_by_thread_id(&conn, "t").unwrap();
    assert!(thread_activities::list_by_thread_id(&conn, "t", None, None).unwrap().is_empty());
}

// ---------------------------------------------------------------- sessions, approvals, state

#[test]
fn sessions_approvals_and_state_round_trip() {
    let conn = fresh();
    let session = thread_sessions::ProjectionThreadSession {
        thread_id: "t".into(),
        status: "running".into(),
        provider_name: Some("codex".into()),
        provider_instance_id: Some("codex".into()),
        runtime_mode: "full-access".into(),
        active_turn_id: Some("turn-1".into()),
        last_error: None,
        updated_at: "2026-03-01T00:00:00.000Z".into(),
    };
    thread_sessions::upsert(&conn, &session).unwrap();
    assert_eq!(thread_sessions::get_by_thread_id(&conn, "t").unwrap(), Some(session));
    thread_sessions::delete_by_thread_id(&conn, "t").unwrap();
    assert_eq!(thread_sessions::get_by_thread_id(&conn, "t").unwrap(), None);

    let approval = |id: &str, status: &str, created: &str| pending_approvals::ProjectionPendingApproval {
        request_id: id.into(),
        thread_id: "t".into(),
        turn_id: None,
        status: status.into(),
        decision: None,
        created_at: created.into(),
        resolved_at: None,
    };
    pending_approvals::upsert(&conn, &approval("r2", "pending", "2026-03-01T00:00:02.000Z")).unwrap();
    pending_approvals::upsert(&conn, &approval("r1", "pending", "2026-03-01T00:00:01.000Z")).unwrap();
    let mut resolved = approval("r3", "resolved", "2026-03-01T00:00:03.000Z");
    resolved.decision = Some("acceptForSession".into());
    resolved.resolved_at = Some("2026-03-01T00:00:04.000Z".into());
    pending_approvals::upsert(&conn, &resolved).unwrap();
    assert_eq!(pending_approvals::count_pending_by_thread_id(&conn, "t").unwrap(), 2);
    assert_eq!(
        pending_approvals::list_by_thread_id(&conn, "t")
            .unwrap()
            .iter()
            .map(|a| a.request_id.as_str())
            .collect::<Vec<_>>(),
        vec!["r1", "r2", "r3"]
    );
    assert_eq!(pending_approvals::get_by_request_id(&conn, "r3").unwrap(), Some(resolved));
    pending_approvals::delete_by_thread_id(&conn, "t").unwrap();
    assert_eq!(pending_approvals::count_pending_by_thread_id(&conn, "t").unwrap(), 0);

    let state = |projector: &str, sequence: i64| projection_state::ProjectionState {
        projector: projector.into(),
        last_applied_sequence: sequence,
        updated_at: "2026-03-01T00:00:00.000Z".into(),
    };
    projection_state::upsert_many(&conn, &[]).unwrap();
    projection_state::upsert_many(&conn, &[state("projection.threads", 3), state("projection.projects", 5)]).unwrap();
    projection_state::upsert(&conn, &state("projection.threads", 9)).unwrap();
    projection_state::upsert_many(&conn, &[state("projection.projects", 7)]).unwrap();
    assert_eq!(
        projection_state::list_all(&conn).unwrap(),
        vec![state("projection.projects", 7), state("projection.threads", 9)]
    );
    assert_eq!(
        projection_state::get_by_projector(&conn, "projection.threads").unwrap(),
        Some(state("projection.threads", 9))
    );
    assert_eq!(projection_state::get_by_projector(&conn, "missing").unwrap(), None);
}

// ---------------------------------------------------------------- turns

fn turn(thread: &str, turn_id: &str, count: Option<i64>, requested: &str) -> turns::ProjectionTurn {
    turns::ProjectionTurn {
        thread_id: thread.into(),
        turn_id: Some(turn_id.into()),
        pending_message_id: None,
        source_proposed_plan_thread_id: None,
        source_proposed_plan_id: None,
        assistant_message_id: None,
        state: "completed".into(),
        requested_at: requested.into(),
        started_at: None,
        completed_at: None,
        checkpoint_turn_count: count,
        checkpoint_ref: count.map(|n| format!("refs/t3/checkpoints/dA/turn/{n}")),
        checkpoint_status: count.map(|_| "ready".into()),
        checkpoint_files: json!([]),
    }
}

#[test]
fn turns_keep_one_pending_start_and_order_checkpoints() {
    let conn = fresh();
    let pending = |message: &str, at: &str| turns::PendingTurnStart {
        thread_id: "t".into(),
        message_id: message.into(),
        source_proposed_plan_thread_id: None,
        source_proposed_plan_id: None,
        requested_at: at.into(),
    };
    assert_eq!(turns::get_pending_turn_start_by_thread_id(&conn, "t").unwrap(), None);
    turns::replace_pending_turn_start(&conn, &pending("m1", "2026-03-01T00:00:01.000Z")).unwrap();
    turns::replace_pending_turn_start(&conn, &pending("m2", "2026-03-01T00:00:02.000Z")).unwrap();
    let pending_rows: i64 = conn
        .raw()
        .query_row("SELECT COUNT(*) FROM projection_turns WHERE turn_id IS NULL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(pending_rows, 1);
    assert_eq!(
        turns::get_pending_turn_start_by_thread_id(&conn, "t").unwrap(),
        Some(pending("m2", "2026-03-01T00:00:02.000Z"))
    );

    // Nested inside an outer transaction, the replacement is a savepoint and rolls back with it.
    let result: Result<(), DbError> = conn.transaction(|conn| {
        turns::replace_pending_turn_start(conn, &pending("m3", "2026-03-01T00:00:03.000Z"))?;
        Err(DbError::decode("test", "abort"))
    });
    assert!(result.is_err());
    assert_eq!(turns::get_pending_turn_start_by_thread_id(&conn, "t").unwrap().unwrap().message_id, "m2");

    turns::upsert_by_turn_id(&conn, &turn("t", "turn-b", Some(2), "2026-03-01T00:00:05.000Z")).unwrap();
    turns::upsert_by_turn_id(&conn, &turn("t", "turn-a", Some(1), "2026-03-01T00:00:06.000Z")).unwrap();
    turns::upsert_by_turn_id(&conn, &turn("t", "turn-c", None, "2026-03-01T00:00:04.000Z")).unwrap();
    let listed: Vec<Option<String>> = turns::list_by_thread_id(&conn, "t").unwrap().into_iter().map(|t| t.turn_id).collect();
    assert_eq!(listed, vec![Some("turn-a".into()), Some("turn-b".into()), None, Some("turn-c".into())]);

    // turn-c takes count 2: clear the conflict first, as the projector does.
    turns::clear_checkpoint_turn_conflict(&conn, "t", "turn-c", 2).unwrap();
    turns::upsert_by_turn_id(&conn, &turn("t", "turn-c", Some(2), "2026-03-01T00:00:04.000Z")).unwrap();
    let b = turns::get_by_turn_id(&conn, "t", "turn-b").unwrap().unwrap();
    assert_eq!((b.checkpoint_turn_count, b.checkpoint_ref, b.checkpoint_status), (None, None, None));
    assert_eq!(turns::get_by_turn_id(&conn, "t", "turn-c").unwrap().unwrap().checkpoint_turn_count, Some(2));

    turns::delete_pending_turn_start_by_thread_id(&conn, "t").unwrap();
    assert_eq!(turns::get_pending_turn_start_by_thread_id(&conn, "t").unwrap(), None);
    turns::delete_by_thread_id(&conn, "t").unwrap();
    assert!(turns::list_by_thread_id(&conn, "t").unwrap().is_empty());
}

// ---------------------------------------------------------------- provider session runtime

fn runtime(thread: &str, payload: Option<Value>) -> provider_session_runtime::ProviderSessionRuntime {
    provider_session_runtime::ProviderSessionRuntime {
        thread_id: thread.into(),
        provider_name: "codex".into(),
        provider_instance_id: None,
        adapter_key: "codex".into(),
        runtime_mode: "full-access".into(),
        status: "running".into(),
        last_seen_at: "2026-06-20T00:00:00.000Z".into(),
        resume_cursor: Some(json!({ "threadId": "codex-thread" })),
        runtime_payload: payload,
    }
}

#[test]
fn provider_runtime_keeps_imported_transcripts_across_upserts() {
    use provider_session_runtime::{OnConflict, *};
    let conn = fresh();
    upsert(
        &conn,
        &runtime("t", Some(json!({ "cwd": "/a", "importedTranscripts": [{ "stale": true }] }))),
        OnConflict::Update,
    )
    .unwrap();
    // The payload's own importedTranscripts are dropped on insert.
    assert_eq!(get_by_thread_id(&conn, "t").unwrap().unwrap().runtime_payload, Some(json!({ "cwd": "/a" })));
    let source = json!({ "provider": "codex", "providerInstanceId": "codex", "providerSessionId": "s1", "filePath": "/x.jsonl", "size": 1, "mtimeMs": null, "device": 1, "inode": null });
    record_imported_transcript(&conn, "t", &source).unwrap();
    let replacement = json!({ "provider": "codex", "providerInstanceId": "codex", "providerSessionId": "s1", "filePath": "/x.jsonl", "size": 2, "mtimeMs": null, "device": 1, "inode": null });
    record_imported_transcript(&conn, "t", &replacement).unwrap();
    upsert(&conn, &runtime("t", Some(json!({ "cwd": "/b" }))), OnConflict::Update).unwrap();
    let stored = get_by_thread_id(&conn, "t").unwrap().unwrap();
    assert_eq!(stored.runtime_payload, Some(json!({ "cwd": "/b", "importedTranscripts": [replacement] })));
    // onConflict "ignore" leaves an existing row alone.
    let mut other = runtime("t", None);
    other.status = "stopped".into();
    upsert(&conn, &other, OnConflict::Ignore).unwrap();
    assert_eq!(get_by_thread_id(&conn, "t").unwrap().unwrap().status, "running");
    upsert(&conn, &runtime("u", None), OnConflict::Ignore).unwrap();
    upsert(&conn, &other.clone(), OnConflict::Update).unwrap();
    assert_eq!(list(&conn, false).unwrap().len(), 2);
    assert_eq!(list(&conn, true).unwrap().iter().map(|r| r.thread_id.as_str()).collect::<Vec<_>>(), vec!["u"]);
    delete_by_thread_id(&conn, "u").unwrap();
    assert!(get_by_thread_id(&conn, "u").unwrap().is_none());
}

#[test]
fn provider_runtime_skips_undecodable_rows_and_correlates_sql_failures() {
    use provider_session_runtime::*;
    let conn = fresh();
    conn.execute(
        "INSERT INTO provider_session_runtime (thread_id, provider_name, provider_instance_id, adapter_key, runtime_mode, status, last_seen_at, resume_cursor_json, runtime_payload_json) VALUES ('thread-correlation', 'codex', NULL, 'codex', 'invalid-runtime-mode', 'running', '2026-06-20T00:00:00.000Z', NULL, '{\"secret\":\"runtime-payload-secret-sentinel\"}')",
        [],
    )
    .unwrap();
    let mut valid = runtime("thread-valid", None);
    valid.resume_cursor = None;
    upsert(&conn, &valid, OnConflict::Update).unwrap();
    assert_eq!(
        list(&conn, false).unwrap().iter().map(|r| r.thread_id.as_str()).collect::<Vec<_>>(),
        vec!["thread-valid"]
    );
    let error = get_by_thread_id(&conn, "thread-correlation").unwrap_err();
    assert!(error.is_decode());
    assert_eq!(error.correlation(), Some(&Correlation::ThreadId("thread-correlation".into())));

    conn.execute_batch("DROP TABLE provider_session_runtime").unwrap();
    let error = upsert(
        &conn,
        &runtime("thread-correlation", Some(json!({ "secret": "runtime-payload-secret-sentinel" }))),
        OnConflict::Update,
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "SQL error in ProviderSessionRuntimeRepository.upsert:query");
    assert_eq!(error.correlation(), Some(&Correlation::ThreadId("thread-correlation".into())));
}

// ---------------------------------------------------------------- auth

fn session_input(id: &str, subject: &str, method: &str) -> auth_sessions::CreateAuthSession {
    auth_sessions::CreateAuthSession {
        session_id: id.into(),
        subject: subject.into(),
        scopes: vec!["access:read".into()],
        method: method.into(),
        client: auth_sessions::ClientMetadata {
            label: None,
            ip_address: None,
            user_agent: None,
            device_type: "desktop".into(),
            os: None,
            browser: None,
        },
        issued_at: ts("2026-06-20T00:00:00.000Z"),
        expires_at: ts("2027-06-20T00:00:00.000Z"),
    }
}

#[test]
fn auth_sessions_lifecycle() {
    use auth_sessions::*;
    let conn = fresh();
    let now = ts("2026-06-21T00:00:00.000Z");
    create(&conn, &session_input("s1", "owner", "browser-session-cookie")).unwrap();
    create_if_absent(&conn, &session_input("s1", "someone-else", "browser-session-cookie")).unwrap();
    assert_eq!(get_by_id(&conn, "s1").unwrap().unwrap().subject, "owner");
    assert!(create(&conn, &session_input("s1", "x", "browser-session-cookie")).unwrap_err().is_constraint());
    let stored: (String, String) = conn
        .raw()
        .query_row("SELECT issued_at, scopes FROM auth_sessions WHERE session_id = 's1'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(stored, ("2026-06-20T00:00:00.000Z".into(), r#"["access:read"]"#.into()));

    let revoked = create_replacing_active(&conn, &session_input("s2", "owner", "browser-session-cookie"), now).unwrap();
    assert_eq!(revoked, vec!["s1".to_string()]);
    assert_eq!(get_by_id(&conn, "s1").unwrap().unwrap().revoked_at, Some(now));
    create(&conn, &session_input("s3", "cli", "bearer-access-token")).unwrap();
    let active: Vec<String> = list_active(&conn, now, &[]).unwrap().into_iter().map(|s| s.session_id).collect();
    assert_eq!(active, vec!["s3".to_string(), "s2".to_string()]);

    // An expired session still lists while it is connected.
    let mut expired = session_input("s4", "old", "browser-session-cookie");
    expired.expires_at = ts("2026-06-20T12:00:00.000Z");
    create(&conn, &expired).unwrap();
    assert!(!list_active(&conn, now, &[]).unwrap().iter().any(|s| s.session_id == "s4"));
    assert!(list_active(&conn, now, &["s4".into()]).unwrap().iter().any(|s| s.session_id == "s4"));

    set_last_connected_at(&conn, "s2", now).unwrap();
    set_client_connection(&conn, "s2", Some("web"), Some("1.2.3")).unwrap();
    set_client_connection(&conn, "s2", None, Some("1.2.4")).unwrap();
    let surface: (Option<String>, Option<String>) = conn
        .raw()
        .query_row(
            "SELECT client_surface, client_app_version FROM auth_sessions WHERE session_id = 's2'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(surface, (Some("web".into()), Some("1.2.4".into())));
    assert_eq!(get_by_id(&conn, "s2").unwrap().unwrap().last_connected_at, Some(now));

    assert!(revoke(&conn, "s3", now).unwrap());
    assert!(!revoke(&conn, "s3", now).unwrap());
    let others = revoke_all_except(&conn, "s2", now).unwrap();
    assert_eq!(others, vec!["s4".to_string()]);
    assert_eq!(list_active(&conn, now, &[]).unwrap().len(), 1);
}

#[test]
fn auth_errors_are_correlated_without_sensitive_fields() {
    use auth_sessions::*;
    let conn = fresh();
    let now = ts("2026-06-21T00:00:00.000Z");
    create(
        &conn,
        &session_input("session-correlation", "session-subject-secret-sentinel", "browser-session-cookie"),
    )
    .unwrap();
    conn.execute(
        "UPDATE auth_sessions SET scopes = 'session-scopes-secret-sentinel' WHERE session_id = 'session-correlation'",
        [],
    )
    .unwrap();
    let error = list_active(&conn, now, &[]).unwrap_err();
    assert!(error.is_decode());
    assert_eq!(error.correlation(), Some(&Correlation::SessionId("session-correlation".into())));
    let message = error.to_string();
    assert!(message.starts_with("Decode error in AuthSessionRepository.listActive:decodeRows: "));
    assert!(!message.contains("secret-sentinel"));

    conn.execute_batch("DROP TABLE auth_sessions").unwrap();
    let error = create(
        &conn,
        &session_input("session-correlation", "session-subject-secret-sentinel", "browser-session-cookie"),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "SQL error in AuthSessionRepository.create:query");
    assert_eq!(error.correlation(), Some(&Correlation::SessionId("session-correlation".into())));
    let error = revoke_all_except(&conn, "current-session-correlation", now).unwrap_err();
    assert_eq!(error.to_string(), "SQL error in AuthSessionRepository.revokeAllExcept:query");
    assert_eq!(error.correlation(), Some(&Correlation::CurrentSessionId("current-session-correlation".into())));

    // Pairing links.
    conn.execute(
        "INSERT INTO auth_pairing_links (id, credential, method, scopes, subject, label, proof_key_thumbprint, created_at, expires_at, consumed_at, revoked_at) VALUES ('pairing-link-correlation', 'pairing-credential-secret-sentinel', 'one-time-token', 'pairing-scopes-secret-sentinel', 'pairing-subject-secret-sentinel', NULL, NULL, '2026-06-20T00:00:00.000Z', '2027-06-20T00:00:00.000Z', NULL, NULL)",
        [],
    )
    .unwrap();
    let error = auth_pairing_links::get_by_credential(&conn, "pairing-credential-secret-sentinel").unwrap_err();
    assert!(error.is_decode());
    assert_eq!(error.correlation(), Some(&Correlation::PairingLinkId("pairing-link-correlation".into())));
    assert!(error
        .to_string()
        .starts_with("Decode error in AuthPairingLinkRepository.getByCredential:decodeRow: "));
    assert!(!error.to_string().contains("secret-sentinel"));
    conn.execute_batch("DROP TABLE auth_pairing_links").unwrap();
    let error = auth_pairing_links::revoke(&conn, "pairing-link-correlation", now).unwrap_err();
    assert_eq!(error.correlation(), Some(&Correlation::PairingLinkId("pairing-link-correlation".into())));
    assert!(!error.to_string().contains("secret-sentinel"));
}

#[test]
fn pairing_links_consume_once_and_classify_failures() {
    use auth_pairing_links::*;
    let conn = fresh();
    let created = ts("2026-06-20T00:00:00.000Z");
    let link = |id: &str, credential: &str, proof: Option<&str>, expires: &str| CreateAuthPairingLink {
        id: id.into(),
        credential: credential.into(),
        method: "one-time-token".into(),
        scopes: vec!["orchestration:read".into()],
        subject: "owner".into(),
        label: Some("Phone".into()),
        proof_key_thumbprint: proof.map(Into::into),
        created_at: created,
        expires_at: ts(expires),
    };
    create(&conn, &link("l1", "c1", None, "2026-06-21T00:00:00.000Z")).unwrap();
    create(&conn, &link("l2", "c2", Some("thumb"), "2026-06-21T00:00:00.000Z")).unwrap();
    create(&conn, &link("l3", "c3", None, "2026-06-20T01:00:00.000Z")).unwrap();
    create(&conn, &link("l4", "c4", None, "2026-06-21T00:00:00.000Z")).unwrap();
    let now = ts("2026-06-20T12:00:00.000Z");
    assert_eq!(
        list_active(&conn, now).unwrap().iter().map(|l| l.id.as_str()).collect::<Vec<_>>(),
        vec!["l4", "l2", "l1"]
    );

    let consumed = consume(&conn, "c1", None, now).unwrap().unwrap();
    assert_eq!(consumed.id, "l1");
    assert_eq!(consumed.consumed_at, Some(now));
    assert_eq!(consume(&conn, "c1", None, now).unwrap(), Err(ConsumeFailure::Unknown));
    assert_eq!(consume(&conn, "missing", None, now).unwrap(), Err(ConsumeFailure::Unknown));
    assert_eq!(consume(&conn, "c2", Some("other"), now).unwrap(), Err(ConsumeFailure::ProofKeyMismatch));
    assert_eq!(consume(&conn, "c2", None, now).unwrap(), Err(ConsumeFailure::ProofKeyMismatch));
    assert_eq!(consume(&conn, "c2", Some("thumb"), now).unwrap().unwrap().id, "l2");
    assert_eq!(consume(&conn, "c3", None, now).unwrap(), Err(ConsumeFailure::Expired));
    assert!(revoke(&conn, "l4", now).unwrap());
    assert!(!revoke(&conn, "l4", now).unwrap());
    assert_eq!(consume(&conn, "c4", None, now).unwrap(), Err(ConsumeFailure::Unavailable));
    assert!(list_active(&conn, now).unwrap().is_empty());
}

// ---------------------------------------------------------------- files viewed

#[test]
fn files_viewed_set_and_list_with_truncation() {
    use pull_request_files_viewed::*;
    let conn = fresh();
    let scope = FilesViewedScope {
        provider: "gitlab".into(),
        host: "gitlab.com".into(),
        repository: "group/project".into(),
        number: 3,
        viewer: "".into(),
    };
    set(
        &conn,
        &scope,
        &[
            FileViewedChange {
                path: "b.rs".into(),
                revision: Some("r1".into()),
                viewed: true,
            },
            FileViewedChange {
                path: "a.rs".into(),
                revision: None,
                viewed: true,
            },
        ],
        "2026-06-20T00:00:00.000Z",
    )
    .unwrap();
    set(
        &conn,
        &scope,
        &[
            FileViewedChange {
                path: "b.rs".into(),
                revision: Some("r2".into()),
                viewed: true,
            },
            FileViewedChange {
                path: "a.rs".into(),
                revision: None,
                viewed: false,
            },
        ],
        "2026-06-20T00:00:01.000Z",
    )
    .unwrap();
    let page = list(&conn, &scope).unwrap();
    assert_eq!(
        page,
        FilesViewedPage {
            files: vec![FileViewedMark {
                path: "b.rs".into(),
                revision: Some("r2".into())
            }],
            truncated: false
        }
    );
    let many: Vec<FileViewedChange> = (0..501)
        .map(|i| FileViewedChange {
            path: format!("f{i:04}"),
            revision: None,
            viewed: true,
        })
        .collect();
    set(&conn, &scope, &many, "2026-06-20T00:00:02.000Z").unwrap();
    let page = list(&conn, &scope).unwrap();
    assert!(page.truncated);
    assert_eq!(page.files.len(), 500);
    assert_eq!(page.files[0].path, "b.rs".to_string().min("f0000".into()));
    // Another viewer sees nothing.
    let other = FilesViewedScope {
        viewer: "someone".into(),
        ..scope
    };
    assert!(list(&conn, &other).unwrap().files.is_empty());
}
