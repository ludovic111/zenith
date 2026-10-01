//! Ports of the decider unit tests in `apps/server/src/orchestration/`:
//! `decider.active-order.test.ts`, `decider.autoSettleSet.test.ts`, `decider.delete.test.ts`,
//! `decider.import.test.ts`, `decider.pinned.test.ts`, `decider.projectScripts.test.ts`,
//! `decider.projectThreadEnvMode.test.ts` and `commandInvariants.test.ts`.
//!
//! The TS tests run under `it.effect`, whose `TestClock` starts at the epoch: the decider's
//! "now" is `1970-01-01T00:00:00.000Z` unless a test moves the clock.

mod common;

use common::*;
use serde_json::{json, Value};

/// What `DateTime.now` reads under the Effect `TestClock` before any `setTime`.
const EPOCH: &str = "1970-01-01T00:00:00.000Z";

/// `expect(actual).toMatchObject(expected)`: every key of `expected` must match in `actual`
/// (recursively); arrays must have the same length and match element by element.
fn matches_object(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => expected
            .iter()
            .all(|(key, value)| actual.get(key).is_some_and(|other| matches_object(other, value))),
        (Value::Array(actual), Value::Array(expected)) => {
            actual.len() == expected.len() && actual.iter().zip(expected).all(|(left, right)| matches_object(left, right))
        }
        (Value::Number(left), Value::Number(right)) => left.as_f64() == right.as_f64(),
        (left, right) => left == right,
    }
}

#[track_caller]
fn assert_matches(actual: &Value, expected: &Value) {
    assert!(matches_object(actual, expected), "expected\n{actual:#}\nto match\n{expected:#}");
}

fn types(events: &[Value]) -> Vec<&str> {
    events.iter().map(|event| event["type"].as_str().unwrap()).collect()
}

// ---------------------------------------------------------------------------------------------
mod command_invariants {
    //! `commandInvariants.test.ts`.

    use super::*;
    use zc_contracts::{OrchestrationReadModel, ProjectId, ThreadId};
    use zc_orchestration::invariants::{list_threads_by_project_id, require_thread, require_thread_absent};

    const NOW: &str = "2026-01-01T00:00:00.000Z";

    fn thread(id: &str, project_id: &str, title: &str) -> Value {
        json!({
            "id": id,
            "projectId": project_id,
            "title": title,
            "modelSelection": {"instanceId": "codex", "model": "gpt-5-codex"},
            "interactionMode": "default",
            "runtimeMode": "full-access",
            "branch": null,
            "worktreePath": null,
            "pullRequests": [],
            "createdAt": NOW,
            "updatedAt": NOW,
            "archivedAt": null,
            "settledOverride": null,
            "settledAt": null,
            "latestTurn": null,
            "messages": [],
            "session": null,
            "activities": [],
            "proposedPlans": [],
            "checkpoints": [],
            "deletedAt": null
        })
    }

    fn project(id: &str, title: &str, root: &str) -> Value {
        json!({
            "id": id,
            "title": title,
            "workspaceRoot": root,
            "defaultModelSelection": {"instanceId": "codex", "model": "gpt-5-codex"},
            "scripts": [],
            "createdAt": NOW,
            "updatedAt": NOW,
            "deletedAt": null
        })
    }

    fn read_model_json() -> Value {
        json!({
            "snapshotSequence": 2,
            "updatedAt": NOW,
            "projects": [
                project("project-a", "Project A", "/tmp/project-a"),
                project("project-b", "Project B", "/tmp/project-b")
            ],
            "threads": [
                thread("thread-1", "project-a", "Thread A"),
                thread("thread-2", "project-b", "Thread B")
            ]
        })
    }

    fn model() -> OrchestrationReadModel {
        read_model(read_model_json())
    }

    #[test]
    fn lists_threads_by_project() {
        let model = model();
        let project_id = ProjectId::new("project-b");
        let ids: Vec<String> = list_threads_by_project_id(&model, &project_id).map(|thread| thread.id.to_string()).collect();
        assert_eq!(ids, ["thread-2"]);
    }

    #[test]
    fn requires_existing_thread() {
        let model = model();
        let thread = require_thread(&model, "thread.turn.start", &ThreadId::new("thread-1")).unwrap();
        assert_eq!(thread.id.to_string(), "thread-1");

        let error = require_thread(&model, "thread.turn.start", &ThreadId::new("missing")).unwrap_err();
        assert!(error.to_string().contains("does not exist"), "{error}");
    }

    #[test]
    fn requires_missing_thread_for_create_flows() {
        let model = model();
        require_thread_absent(&model, "thread.create", &ThreadId::new("thread-3")).unwrap();

        let error = require_thread_absent(&model, "thread.create", &ThreadId::new("thread-1")).unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error}");
    }

    #[test]
    fn lets_a_draft_retry_re_create_a_thread_id_after_its_first_attempt_was_deleted() {
        let mut value = read_model_json();
        for thread in value["threads"].as_array_mut().unwrap() {
            if thread["id"] == "thread-1" {
                thread["deletedAt"] = json!(NOW);
                thread["updatedAt"] = json!(NOW);
            }
        }
        let after_rollback = read_model(value);
        assert_eq!(require_thread_absent(&after_rollback, "thread.create", &ThreadId::new("thread-1")), Ok(()));
    }
}

// ---------------------------------------------------------------------------------------------
mod active_order {
    //! `decider.active-order.test.ts`.

    use super::*;
    use zc_contracts::OrchestrationReadModel;

    const NOW: &str = "2026-01-01T00:00:00.000Z";
    // The Effect test clock starts at the epoch.
    const BEFORE_NOW: &str = "1969-12-30T00:00:00.000Z";
    const SNOOZED_AT: &str = "1969-12-31T00:00:00.000Z";
    const FUTURE_WAKE: &str = "1970-01-02T00:00:00.000Z";
    const THREAD_ID: &str = "thread-1";

    fn thread_value(overrides: Value) -> Value {
        let mut thread = thread_json(
            THREAD_ID,
            "project-1",
            NOW,
            json!({
                "unsettledAt": null,
                "activeOrderKey": null,
                "snoozedUntil": null,
                "snoozedAt": null,
                "pinnedAt": null,
                "pinOrderKey": null
            }),
        );
        merge(&mut thread, overrides);
        thread
    }

    fn make_read_model(overrides: Value) -> OrchestrationReadModel {
        read_model(json!({
            "snapshotSequence": 0,
            "projects": [],
            "threads": [thread_value(overrides)],
            "updatedAt": NOW
        }))
    }

    fn reorder_command(order_key: &str) -> Value {
        json!({
            "type": "thread.active.reorder",
            "commandId": "cmd-active-reorder",
            "threadId": THREAD_ID,
            "orderKey": order_key
        })
    }

    #[test]
    fn persists_changed_and_repeated_slots_without_changing_thread_activity_timestamps() {
        let env = TestEnv::at(EPOCH);
        let mut model = make_read_model(json!({"unsettledAt": BEFORE_NOW}));
        for order_key in ["m", "m", "g"] {
            let events = decide(&env, reorder_command(order_key), &model).unwrap();
            assert_eq!(events.len(), 1);
            assert_matches(
                &events[0],
                &json!({
                    "type": "thread.meta-updated",
                    "payload": {"threadId": THREAD_ID, "activeOrderKey": order_key, "updatedAt": NOW}
                }),
            );
            apply_decided(&mut model, &events);
            assert_matches(
                &to_json(&model.threads[0]),
                &json!({
                    "activeOrderKey": order_key,
                    "updatedAt": NOW,
                    "createdAt": NOW,
                    "unsettledAt": BEFORE_NOW
                }),
            );
        }
    }

    fn rejects_reordering(overrides: Value) {
        let env = TestEnv::at(EPOCH);
        let error = decide(&env, reorder_command("m"), &make_read_model(overrides)).unwrap_err();
        assert_eq!(tag(&error), "OrchestrationCommandInvariantError");
    }

    #[test]
    fn rejects_reordering_an_archived_thread() {
        rejects_reordering(json!({"archivedAt": NOW}));
    }

    #[test]
    fn rejects_reordering_a_deleted_thread() {
        rejects_reordering(json!({"deletedAt": NOW}));
    }

    #[test]
    fn rejects_reordering_a_pinned_thread() {
        rejects_reordering(json!({"pinnedAt": NOW}));
    }

    #[test]
    fn rejects_reordering_a_settled_thread() {
        rejects_reordering(json!({"settledOverride": "settled", "settledAt": NOW}));
    }

    /// Projects the single decided event on the model and checks the thread only changed its
    /// `activeOrderKey`.
    fn assert_reorder_only_moves_the_slot(model: OrchestrationReadModel) {
        let env = TestEnv::at(EPOCH);
        let events = decide(&env, reorder_command("m"), &model).unwrap();
        assert_eq!(events.len(), 1);
        let mut expected = to_json(&model.threads[0]);
        expected["activeOrderKey"] = json!("m");
        for event in &events {
            let mut projected = model.clone();
            let mut event = event.clone();
            event["sequence"] = json!(1);
            apply(&mut projected, event);
            let actual = to_json(&projected.threads[0]);
            assert_eq!(actual, expected, "{:?}", json_diff(&expected, &actual, 10));
        }
    }

    #[test]
    fn reorders_a_running_thread_without_affecting_its_session() {
        assert_reorder_only_moves_the_slot(make_read_model(json!({
            "session": {
                "threadId": THREAD_ID,
                "status": "running",
                "providerName": "codex",
                "runtimeMode": "full-access",
                "activeTurnId": null,
                "lastError": null,
                "updatedAt": NOW
            }
        })));
    }

    #[test]
    fn changes_a_snoozed_threads_retained_slot_without_waking_it_or_changing_timestamps() {
        assert_reorder_only_moves_the_slot(make_read_model(json!({
            "activeOrderKey": "g",
            "snoozedAt": SNOOZED_AT,
            "snoozedUntil": FUTURE_WAKE,
            "unsettledAt": BEFORE_NOW
        })));
    }

    #[test]
    fn keeps_placement_through_metadata_pin_and_snooze_then_resets_it_on_settlement() {
        let env = TestEnv::at(EPOCH);
        let mut model = make_read_model(json!({}));
        let steps: [(Value, Option<&str>); 9] = [
            (reorder_command("m"), Some("m")),
            (json!({"type": "thread.meta.update", "title": "Renamed"}), Some("m")),
            (json!({"type": "thread.pin", "orderKey": "g"}), Some("m")),
            (json!({"type": "thread.snooze", "snoozedUntil": FUTURE_WAKE}), Some("m")),
            (json!({"type": "thread.unsnooze", "reason": "user"}), Some("m")),
            (json!({"type": "thread.unpin"}), Some("m")),
            (json!({"type": "thread.settle"}), None),
            (json!({"type": "thread.unsettle", "reason": "user"}), None),
            (json!({"type": "thread.active.reorder", "orderKey": "s"}), Some("s")),
        ];
        for (index, (step, expected_key)) in steps.into_iter().enumerate() {
            let mut command = step;
            merge(&mut command, json!({"commandId": format!("lifecycle-{index}"), "threadId": THREAD_ID}));
            let command_type = command["type"].as_str().unwrap().to_owned();
            let events = decide(&env, command, &model).unwrap();
            apply_decided(&mut model, &events);
            assert_eq!(
                model.threads[0].active_order_key.clone().flatten().map(|key| key.to_string()).as_deref(),
                expected_key,
                "{command_type}"
            );
        }
        assert_matches(
            &to_json(&model.threads[0]),
            &json!({
                "title": "Renamed",
                "settledOverride": "active",
                "settledAt": null,
                "snoozedUntil": null,
                "pinnedAt": null
            }),
        );
    }
}

// ---------------------------------------------------------------------------------------------
mod auto_settle_set {
    //! `decider.autoSettleSet.test.ts`.

    use super::*;
    use zc_contracts::OrchestrationReadModel;

    const NOW: &str = "2026-01-01T00:00:00.000Z";
    const DISABLED_AT: &str = "2025-12-30T00:00:00.000Z";

    fn make_read_model(auto_settle_disabled_at: Option<&str>, settled_override: Option<&str>) -> OrchestrationReadModel {
        read_model(json!({
            "snapshotSequence": 0,
            "projects": [],
            "threads": [thread_json("thread-1", "project-1", NOW, json!({
                "settledOverride": settled_override,
                "settledAt": if settled_override == Some("settled") { json!(NOW) } else { json!(null) },
                "autoSettleDisabledAt": auto_settle_disabled_at
            }))],
            "updatedAt": NOW
        }))
    }

    fn auto_settle_set(command_id: &str, enabled: bool) -> Value {
        json!({
            "type": "thread.auto-settle.set",
            "commandId": command_id,
            "threadId": "thread-1",
            "enabled": enabled
        })
    }

    #[test]
    fn turning_auto_settle_off_stamps_auto_settle_disabled_at_and_updated_at_together() {
        let env = TestEnv::at(EPOCH);
        let events = decide(&env, auto_settle_set("cmd-off", false), &make_read_model(None, None)).unwrap();
        let event = &events[0];
        assert_eq!(event["type"], "thread.auto-settle-set");
        assert_eq!(event["payload"]["autoSettleDisabledAt"], event["payload"]["updatedAt"]);
        assert_ne!(event["payload"]["updatedAt"], NOW);
    }

    #[test]
    fn turning_it_off_again_keeps_the_original_stamp_and_updated_at() {
        let env = TestEnv::at(EPOCH);
        let events = decide(&env, auto_settle_set("cmd-off-again", false), &make_read_model(Some(DISABLED_AT), None)).unwrap();
        let event = &events[0];
        assert_eq!(event["type"], "thread.auto-settle-set");
        assert_eq!(event["payload"]["autoSettleDisabledAt"], DISABLED_AT);
        assert_eq!(event["payload"]["updatedAt"], NOW);
    }

    #[test]
    fn turning_auto_settle_back_on_clears_the_stamp() {
        let env = TestEnv::at(EPOCH);
        let events = decide(&env, auto_settle_set("cmd-on", true), &make_read_model(Some(DISABLED_AT), None)).unwrap();
        let event = &events[0];
        assert_eq!(event["type"], "thread.auto-settle-set");
        assert_eq!(event["payload"].get("autoSettleDisabledAt"), Some(&Value::Null));
        assert_ne!(event["payload"]["updatedAt"], NOW);
    }

    #[test]
    fn automatic_settlement_is_rejected_while_auto_settle_is_off() {
        let env = TestEnv::at(EPOCH);
        let result = decide(
            &env,
            json!({
                "type": "thread.auto-settle",
                "commandId": "cmd-auto",
                "threadId": "thread-1",
                "snapshotSequence": 0,
                "settledAt": NOW
            }),
            &make_read_model(Some(DISABLED_AT), None),
        );
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn a_manual_settle_still_works_while_auto_settle_is_off() {
        let env = TestEnv::at(EPOCH);
        let events = decide(
            &env,
            json!({"type": "thread.settle", "commandId": "cmd-manual", "threadId": "thread-1"}),
            &make_read_model(Some(DISABLED_AT), None),
        )
        .unwrap();
        assert_eq!(events[0]["type"], "thread.settled");
    }
}

// ---------------------------------------------------------------------------------------------
mod delete {
    //! `decider.delete.test.ts`.

    use super::*;
    use zc_contracts::OrchestrationReadModel;

    const NOW: &str = "2026-01-01T00:00:00.000Z";

    fn thread_created(sequence: i64, thread_id: &str, title: &str) -> Value {
        make_event(
            sequence,
            "thread.created",
            NOW,
            thread_id,
            json!({
                "threadId": thread_id,
                "projectId": "project-delete",
                "title": title,
                "modelSelection": {"instanceId": "codex", "model": "gpt-5-codex"},
                "interactionMode": "default",
                "runtimeMode": "approval-required",
                "branch": null,
                "worktreePath": null,
                "createdAt": NOW,
                "updatedAt": NOW
            }),
        )
    }

    fn seed_read_model() -> OrchestrationReadModel {
        let mut model = empty_model(NOW);
        apply(
            &mut model,
            make_event(
                1,
                "project.created",
                NOW,
                "project-delete",
                json!({
                    "projectId": "project-delete",
                    "title": "Project Delete",
                    "workspaceRoot": "/tmp/project-delete",
                    "defaultModelSelection": null,
                    "scripts": [],
                    "createdAt": NOW,
                    "updatedAt": NOW
                }),
            ),
        );
        apply(&mut model, thread_created(2, "thread-delete-1", "Thread Delete 1"));
        apply(&mut model, thread_created(3, "thread-delete-2", "Thread Delete 2"));
        model
    }

    /// `normalizeDeleteEvent`.
    fn normalize_delete_events(events: &[Value]) -> Vec<Value> {
        events
            .iter()
            .map(|entry| {
                let payload = match entry["type"].as_str() {
                    Some("thread.deleted") => json!({"threadId": entry["payload"]["threadId"]}),
                    Some("project.deleted") => json!({"projectId": entry["payload"]["projectId"]}),
                    _ => return entry.clone(),
                };
                json!({
                    "type": entry["type"],
                    "aggregateKind": entry["aggregateKind"],
                    "aggregateId": entry["aggregateId"],
                    "commandId": entry["commandId"],
                    "correlationId": entry["correlationId"],
                    "payload": payload
                })
            })
            .collect()
    }

    #[test]
    fn rejects_deleting_a_non_empty_project_without_force() {
        let env = TestEnv::at(EPOCH);
        let model = seed_read_model();
        let error = decide(
            &env,
            json!({
                "type": "project.delete",
                "commandId": "cmd-project-delete-no-force",
                "projectId": "project-delete"
            }),
            &model,
        )
        .unwrap_err();
        assert!(error.to_string().contains("cannot be deleted without force=true"), "{error}");
    }

    #[test]
    fn reuses_thread_delete_semantics_when_force_deleting_a_non_empty_project() {
        let env = TestEnv::at(EPOCH);
        let model = seed_read_model();
        let forced = decide(
            &env,
            json!({
                "type": "project.delete",
                "commandId": "cmd-project-delete-force",
                "projectId": "project-delete",
                "force": true
            }),
            &model,
        )
        .unwrap();
        assert_eq!(types(&forced), ["thread.deleted", "thread.deleted", "project.deleted"]);

        let mut sequential_model = model.clone();
        let mut sequential_events = Vec::new();
        for next_command in [
            json!({"type": "thread.delete", "commandId": "cmd-project-delete-force", "threadId": "thread-delete-1"}),
            json!({"type": "thread.delete", "commandId": "cmd-project-delete-force", "threadId": "thread-delete-2"}),
            json!({"type": "project.delete", "commandId": "cmd-project-delete-force", "projectId": "project-delete"}),
        ] {
            let events = decide(&env, next_command, &sequential_model).unwrap();
            apply_decided(&mut sequential_model, &events);
            sequential_events.extend(events);
        }

        assert_eq!(normalize_delete_events(&forced), normalize_delete_events(&sequential_events));
    }
}

// ---------------------------------------------------------------------------------------------
mod import {
    //! `decider.import.test.ts`.

    use super::*;
    use zc_contracts::OrchestrationReadModel;

    fn thread_created(sequence: i64, thread_id: &str, title: &str, created_at: &str) -> Value {
        make_event(
            sequence,
            "thread.created",
            created_at,
            thread_id,
            json!({
                "threadId": thread_id,
                "projectId": "project-1",
                "title": title,
                "modelSelection": {"instanceId": "codex", "model": "gpt-5"},
                "runtimeMode": "full-access",
                "interactionMode": "default",
                "branch": null,
                "worktreePath": null,
                "createdAt": created_at,
                "updatedAt": created_at
            }),
        )
    }

    fn model_with_thread(thread_id: &str, title: &str, created_at: &str) -> OrchestrationReadModel {
        let mut model = empty_model(created_at);
        apply(&mut model, thread_created(1, thread_id, title, created_at));
        model
    }

    #[test]
    fn marks_imported_thread_creation_without_changing_live_creation() {
        let env = TestEnv::at(EPOCH);
        let created_at = "2026-08-24T10:00:00.000Z";
        let mut model = empty_model(created_at);
        apply(
            &mut model,
            make_event(
                1,
                "project.created",
                created_at,
                "project-1",
                json!({
                    "projectId": "project-1",
                    "title": "Project",
                    "workspaceRoot": "/tmp/project",
                    "defaultModelSelection": null,
                    "scripts": [],
                    "createdAt": created_at,
                    "updatedAt": created_at
                }),
            ),
        );
        let make_create_command = |thread_id: &str| {
            json!({
                "type": "thread.create",
                "commandId": format!("command-create-{thread_id}"),
                "threadId": thread_id,
                "projectId": "project-1",
                "title": "Imported thread",
                "modelSelection": {"instanceId": "codex", "model": "gpt-5"},
                "runtimeMode": "full-access",
                "interactionMode": "default",
                "branch": null,
                "worktreePath": null,
                "createdAt": created_at
            })
        };

        let mut imported_command = make_create_command("import:codex:session-1");
        imported_command["historyImport"] = json!(true);
        let imported = decide(&env, imported_command, &model).unwrap();
        let live = decide(&env, make_create_command("live-thread"), &model).unwrap();

        assert_eq!(imported.len(), 1);
        assert_matches(&imported[0], &json!({"type": "thread.created", "metadata": {"historyImport": true}}));
        assert_eq!(live.len(), 1);
        assert_matches(&live[0], &json!({"type": "thread.created"}));
        assert!(!matches_object(&live[0], &json!({"metadata": {"historyImport": true}})));
    }

    #[test]
    fn settles_imported_messages_at_the_latest_absolute_timestamp() {
        let env = TestEnv::at(EPOCH);
        let created_at = "2026-08-24T10:30:00.000+02:00";
        let thread_id = "import:codex:session-1";
        let model = model_with_thread(thread_id, "Imported thread", created_at);

        let events = decide(
            &env,
            json!({
                "type": "thread.history.import",
                "commandId": "command-import-history",
                "threadId": thread_id,
                "messages": [
                    {
                        "messageId": format!("{thread_id}:000000"),
                        "role": "user",
                        "text": "Fix the bug",
                        "createdAt": created_at
                    },
                    {
                        "messageId": format!("{thread_id}:000001"),
                        "role": "assistant",
                        "text": "Fixed",
                        "createdAt": "2026-08-24T09:00:00.000Z"
                    }
                ]
            }),
            &model,
        )
        .unwrap();

        assert_matches(
            &Value::Array(events.clone()),
            &json!([
                {
                    "type": "thread.message-sent",
                    "metadata": {"historyImport": true},
                    "payload": {"role": "user", "text": "Fix the bug", "turnId": null, "streaming": false}
                },
                {
                    "type": "thread.message-sent",
                    "metadata": {"historyImport": true},
                    "payload": {"role": "assistant", "text": "Fixed", "turnId": null, "streaming": false}
                },
                {
                    "type": "thread.settled",
                    "metadata": {"historyImport": true},
                    "occurredAt": "2026-08-24T09:00:00.000Z",
                    "payload": {
                        "settledAt": "2026-08-24T09:00:00.000Z",
                        "updatedAt": "2026-08-24T09:00:00.000Z"
                    }
                }
            ]),
        );

        let mut projected = model.clone();
        for (index, event) in events.iter().enumerate() {
            let mut event = event.clone();
            event["sequence"] = json!(index + 2);
            apply(&mut projected, event);
        }
        apply(
            &mut projected,
            make_event(
                5,
                "thread.reverted",
                "2026-08-24T10:02:00.000Z",
                thread_id,
                json!({"threadId": thread_id, "turnCount": 0}),
            ),
        );
        let texts: Vec<&str> = projected.threads[0].messages.iter().map(|message| message.text.as_str()).collect();
        assert_eq!(texts, ["Fix the bug", "Fixed"]);
    }

    #[test]
    fn allows_a_thread_with_a_newly_imported_user_message_to_be_settled() {
        let env = TestEnv::at(EPOCH);
        let created_at = "2026-08-24T10:00:00.000Z";
        env.set_now("2026-08-24T10:00:30.000Z");
        let thread_id = "import:codex:session-1";
        let mut model = model_with_thread(thread_id, "Imported thread", created_at);
        let mut message = make_event(
            2,
            "thread.message-sent",
            created_at,
            thread_id,
            json!({
                "threadId": thread_id,
                "messageId": "import:codex:session-1:0",
                "role": "user",
                "text": "Existing prompt",
                "turnId": null,
                "streaming": false,
                "createdAt": created_at,
                "updatedAt": created_at
            }),
        );
        message["metadata"] = json!({"historyImport": true});
        apply(&mut model, message);

        let events = decide(
            &env,
            json!({
                "type": "thread.settle",
                "commandId": "command-settle-imported-thread",
                "threadId": thread_id
            }),
            &model,
        )
        .unwrap();
        // TS returns a single event here (`toMatchObject` on a non-array).
        assert_eq!(events.len(), 1);
        assert_matches(&events[0], &json!({"type": "thread.settled"}));
    }

    #[test]
    fn rejects_history_import_after_a_client_message_reaches_the_thread() {
        let env = TestEnv::at(EPOCH);
        let created_at = "2026-08-24T10:00:00.000Z";
        let live_message_at = "2026-08-24T10:02:00.000Z";
        let thread_id = "import:codex:client-race";
        let mut model = model_with_thread(thread_id, "Imported thread", created_at);
        apply(
            &mut model,
            make_event(
                2,
                "thread.message-sent",
                live_message_at,
                thread_id,
                json!({
                    "threadId": thread_id,
                    "messageId": "client-race-message",
                    "role": "user",
                    "text": "Start live work",
                    "turnId": null,
                    "streaming": false,
                    "createdAt": live_message_at,
                    "updatedAt": live_message_at
                }),
            ),
        );

        let error = decide(
            &env,
            json!({
                "type": "thread.history.import",
                "commandId": "command-client-race-import",
                "threadId": thread_id,
                "messages": [{
                    "messageId": format!("{thread_id}:000000"),
                    "role": "user",
                    "text": "Old work",
                    "createdAt": created_at
                }]
            }),
            &model,
        )
        .unwrap_err();

        assert_eq!(tag(&error), "OrchestrationCommandInvariantError");
        assert!(error.to_string().contains("must be active and empty"), "{error}");
        assert_eq!(model.threads[0].updated_at, live_message_at);
    }

    fn rejects_history_import_with_an_open_request(request_kind: &str) {
        let env = TestEnv::at(EPOCH);
        let created_at = "2026-08-24T10:00:00.000Z";
        let thread_id = format!("import:codex:{request_kind}");
        let mut model = model_with_thread(&thread_id, "Imported thread", created_at);
        apply(
            &mut model,
            make_event(
                2,
                "thread.activity-appended",
                created_at,
                &thread_id,
                json!({
                    "threadId": thread_id,
                    "activity": {
                        "id": format!("activity-{request_kind}"),
                        "tone": "approval",
                        "kind": request_kind,
                        "summary": "Pending request",
                        "payload": {"requestId": "request-1"},
                        "turnId": null,
                        "createdAt": created_at
                    }
                }),
            ),
        );

        let error = decide(
            &env,
            json!({
                "type": "thread.history.import",
                "commandId": format!("command-import-{request_kind}"),
                "threadId": thread_id,
                "messages": [{
                    "messageId": format!("{thread_id}:000000"),
                    "role": "user",
                    "text": "Old work",
                    "createdAt": created_at
                }]
            }),
            &model,
        )
        .unwrap_err();

        assert_eq!(tag(&error), "OrchestrationCommandInvariantError");
        assert!(error.to_string().contains("must be active and empty"), "{error}");
    }

    #[test]
    fn rejects_history_import_with_an_open_approval_requested_activity() {
        rejects_history_import_with_an_open_request("approval.requested");
    }

    #[test]
    fn rejects_history_import_with_an_open_user_input_requested_activity() {
        rejects_history_import_with_an_open_request("user-input.requested");
    }

    #[test]
    fn rejects_a_live_user_message_in_the_imported_session_namespace() {
        let env = TestEnv::at(EPOCH);
        let created_at = "2026-08-24T10:00:00.000Z";
        let thread_id = "thread-live-message";
        let model = model_with_thread(thread_id, "Live thread", created_at);

        let error = decide(
            &env,
            json!({
                "type": "thread.turn.start",
                "commandId": "command-live-import-id",
                "threadId": thread_id,
                "message": {
                    "messageId": "import:forged-live-message",
                    "role": "user",
                    "text": "Live work",
                    "attachments": []
                },
                "runtimeMode": "full-access",
                "interactionMode": "default",
                "createdAt": created_at
            }),
            &model,
        )
        .unwrap_err();

        assert_eq!(tag(&error), "OrchestrationCommandInvariantError");
        assert!(error.to_string().contains("reserved imported-session namespace"), "{error}");
    }

    #[test]
    fn rejects_live_assistant_messages_in_the_imported_session_namespace() {
        let env = TestEnv::at(EPOCH);
        let created_at = "2026-08-24T10:00:00.000Z";
        let thread_id = "thread-live-assistant-message";
        let model = model_with_thread(thread_id, "Live thread", created_at);

        for command in [
            json!({
                "type": "thread.message.assistant.delta",
                "commandId": "command-live-assistant-delta-import-id",
                "threadId": thread_id,
                "messageId": "import:forged-live-assistant-message",
                "delta": "Live work",
                "createdAt": created_at
            }),
            json!({
                "type": "thread.message.assistant.complete",
                "commandId": "command-live-assistant-complete-import-id",
                "threadId": thread_id,
                "messageId": "import:forged-live-assistant-message",
                "createdAt": created_at
            }),
        ] {
            let command_type = command["type"].clone();
            let error = decide(&env, command, &model).unwrap_err();
            assert_eq!(tag(&error), "OrchestrationCommandInvariantError", "{command_type}");
            assert!(error.to_string().contains("reserved imported-session namespace"), "{command_type}: {error}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
mod pinned {
    //! `decider.pinned.test.ts`.

    use super::*;
    use zc_contracts::OrchestrationReadModel;

    const NOW: &str = "2026-01-01T00:00:00.000Z";
    const PINNED_AT: &str = "1969-12-30T00:00:00.000Z";

    #[derive(Default)]
    struct Input {
        pinned_at: Option<&'static str>,
        pin_order_key: Option<&'static str>,
        archived_at: Option<&'static str>,
        settled_override: Option<&'static str>,
        snoozed_until: Option<&'static str>,
    }

    fn make_read_model(input: Input) -> OrchestrationReadModel {
        read_model(json!({
            "snapshotSequence": 0,
            "projects": [],
            "threads": [thread_json("thread-1", "project-1", NOW, json!({
                "archivedAt": input.archived_at,
                "settledOverride": input.settled_override,
                "settledAt": if input.settled_override == Some("settled") { Some(NOW) } else { None },
                "snoozedUntil": input.snoozed_until,
                "snoozedAt": input.snoozed_until.map(|_| PINNED_AT),
                "pinnedAt": input.pinned_at,
                "pinOrderKey": input.pin_order_key
            }))],
            "updatedAt": NOW
        }))
    }

    fn command_value(command_type: &str, command_id: &str, extra: Value) -> Value {
        let mut command = json!({"type": command_type, "commandId": command_id, "threadId": "thread-1"});
        merge(&mut command, extra);
        command
    }

    fn run(command_type: &str, command_id: &str, extra: Value, input: Input) -> Vec<Value> {
        let env = TestEnv::at(EPOCH);
        decide(&env, command_value(command_type, command_id, extra), &make_read_model(input)).unwrap()
    }

    fn reject(command_type: &str, command_id: &str, extra: Value, input: Input) -> &'static str {
        let env = TestEnv::at(EPOCH);
        let error = decide(&env, command_value(command_type, command_id, extra), &make_read_model(input)).unwrap_err();
        tag(&error)
    }

    #[test]
    fn pins_a_thread_stamping_pinned_at_and_updated_at_together() {
        let events = run("thread.pin", "cmd-pin", json!({}), Input::default());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "thread.pinned");
        assert_eq!(events[0]["payload"]["pinnedAt"], events[0]["payload"]["updatedAt"]);
    }

    #[test]
    fn re_pinning_preserves_the_original_pinned_at_and_updated_at() {
        let events = run(
            "thread.pin",
            "cmd-pin-again",
            json!({}),
            Input {
                pinned_at: Some(PINNED_AT),
                ..Input::default()
            },
        );
        assert_eq!(events[0]["type"], "thread.pinned");
        assert_eq!(events[0]["payload"]["pinnedAt"], PINNED_AT);
        assert_eq!(events[0]["payload"]["updatedAt"], NOW);
    }

    #[test]
    fn unpins_a_pinned_thread() {
        let events = run(
            "thread.unpin",
            "cmd-unpin",
            json!({}),
            Input {
                pinned_at: Some(PINNED_AT),
                ..Input::default()
            },
        );
        assert_eq!(events[0]["type"], "thread.unpinned");
        assert_ne!(events[0]["payload"]["updatedAt"], NOW);
    }

    #[test]
    fn unpinning_an_unpinned_thread_preserves_updated_at() {
        let events = run("thread.unpin", "cmd-unpin-noop", json!({}), Input::default());
        assert_eq!(events[0]["type"], "thread.unpinned");
        assert_eq!(events[0]["payload"]["updatedAt"], NOW);
    }

    #[test]
    fn pinning_a_settled_thread_also_un_settles_it() {
        let events = run(
            "thread.pin",
            "cmd-pin-settled",
            json!({}),
            Input {
                settled_override: Some("settled"),
                ..Input::default()
            },
        );
        assert_eq!(types(&events), ["thread.pinned", "thread.unsettled"]);
        let unsettled = events.iter().find(|entry| entry["type"] == "thread.unsettled").unwrap();
        assert_eq!(unsettled["payload"]["reason"], "user");
    }

    #[test]
    fn pinning_a_snoozed_thread_also_wakes_it() {
        let events = run(
            "thread.pin",
            "cmd-pin-snoozed",
            json!({}),
            Input {
                snoozed_until: Some("1970-01-02T09:00:00.000Z"),
                ..Input::default()
            },
        );
        assert_eq!(types(&events), ["thread.pinned", "thread.unsnoozed"]);
    }

    #[test]
    fn pinning_an_unparked_thread_emits_only_thread_pinned() {
        let events = run("thread.pin", "cmd-pin-plain", json!({}), Input::default());
        assert_eq!(types(&events), ["thread.pinned"]);
    }

    #[test]
    fn settling_a_pinned_thread_also_unpins_it() {
        let events = run(
            "thread.settle",
            "cmd-settle-pinned",
            json!({}),
            Input {
                pinned_at: Some(PINNED_AT),
                ..Input::default()
            },
        );
        assert_eq!(types(&events), ["thread.settled", "thread.unpinned"]);
    }

    #[test]
    fn settling_an_unpinned_thread_emits_no_unpin_event() {
        let events = run("thread.settle", "cmd-settle-unpinned", json!({}), Input::default());
        assert_eq!(types(&events), ["thread.settled"]);
    }

    #[test]
    fn rejects_pinning_an_archived_thread() {
        let tag = reject(
            "thread.pin",
            "cmd-pin-archived",
            json!({}),
            Input {
                archived_at: Some(NOW),
                ..Input::default()
            },
        );
        assert_eq!(tag, "OrchestrationCommandInvariantError");
    }

    #[test]
    fn a_fresh_pin_carries_the_clients_order_key() {
        let events = run("thread.pin", "cmd-pin-keyed", json!({"orderKey": "g"}), Input::default());
        assert_eq!(events[0]["type"], "thread.pinned");
        assert_eq!(events[0]["payload"]["pinOrderKey"], "g");
    }

    #[test]
    fn re_pinning_ignores_the_incoming_order_key_so_raced_pins_cannot_move_a_placed_thread() {
        let events = run(
            "thread.pin",
            "cmd-pin-keyed-again",
            json!({"orderKey": "t"}),
            Input {
                pinned_at: Some(PINNED_AT),
                pin_order_key: Some("g"),
                ..Input::default()
            },
        );
        assert_eq!(events[0]["type"], "thread.pinned");
        // `toBeUndefined`: the key is absent from the payload.
        assert_eq!(events[0]["payload"].get("pinOrderKey"), None, "{}", events[0]["payload"]);
    }

    #[test]
    fn reorders_a_pinned_thread_stamping_the_new_key() {
        let events = run(
            "thread.pin.reorder",
            "cmd-reorder",
            json!({"orderKey": "m"}),
            Input {
                pinned_at: Some(PINNED_AT),
                pin_order_key: Some("g"),
                ..Input::default()
            },
        );
        assert_eq!(events[0]["type"], "thread.pin-reordered");
        assert_eq!(events[0]["payload"]["orderKey"], "m");
        // A real move stamps the command time (the test clock), not the thread's previous
        // updatedAt.
        assert_ne!(events[0]["payload"]["updatedAt"], NOW);
    }

    #[test]
    fn reordering_onto_the_same_key_preserves_updated_at() {
        let events = run(
            "thread.pin.reorder",
            "cmd-reorder-noop",
            json!({"orderKey": "g"}),
            Input {
                pinned_at: Some(PINNED_AT),
                pin_order_key: Some("g"),
                ..Input::default()
            },
        );
        assert_eq!(events[0]["type"], "thread.pin-reordered");
        assert_eq!(events[0]["payload"]["updatedAt"], NOW);
    }

    #[test]
    fn rejects_reordering_an_unpinned_thread() {
        let tag = reject("thread.pin.reorder", "cmd-reorder-unpinned", json!({"orderKey": "m"}), Input::default());
        assert_eq!(tag, "OrchestrationCommandInvariantError");
    }
}

// ---------------------------------------------------------------------------------------------
mod project_scripts {
    //! `decider.projectScripts.test.ts`.

    use super::*;
    use zc_contracts::OrchestrationReadModel;

    const NOW: &str = "2026-01-01T00:00:00.000Z";

    fn project_created(sequence: i64, project_id: &str, title: &str, workspace_root: &str, scripts: Value) -> Value {
        make_event(
            sequence,
            "project.created",
            NOW,
            project_id,
            json!({
                "projectId": project_id,
                "title": title,
                "workspaceRoot": workspace_root,
                "defaultModelSelection": null,
                "scripts": scripts,
                "createdAt": NOW,
                "updatedAt": NOW
            }),
        )
    }

    fn script(id: &str) -> Value {
        json!({
            "id": id,
            "name": "Install dependencies",
            "command": "vp i",
            "icon": "configure",
            "runOnWorktreeCreate": false
        })
    }

    fn project_with_scripts(scripts: Value) -> OrchestrationReadModel {
        let mut model = empty_model(NOW);
        apply(&mut model, project_created(1, "project-scripts", "Scripts", "/tmp/scripts", scripts));
        model
    }

    fn meta_update(command_id: &str, project_id: &str, fields: Value) -> Value {
        let mut command = json!({"type": "project.meta.update", "commandId": command_id, "projectId": project_id});
        merge(&mut command, fields);
        command
    }

    #[test]
    fn emits_empty_scripts_on_project_create() {
        let env = TestEnv::at(EPOCH);
        let model = empty_model(NOW);
        let events = decide(
            &env,
            json!({
                "type": "project.create",
                "commandId": "cmd-project-create-scripts",
                "projectId": "project-scripts",
                "title": "Scripts",
                "workspaceRoot": "/tmp/scripts",
                "createdAt": NOW
            }),
            &model,
        )
        .unwrap();
        assert_eq!(events[0]["type"], "project.created");
        assert_eq!(events[0]["payload"]["scripts"], json!([]));
    }

    #[test]
    fn propagates_scripts_in_project_meta_update_payload() {
        let env = TestEnv::at(EPOCH);
        let model = project_with_scripts(json!([]));
        let scripts = json!([{
            "id": "lint",
            "name": "Lint",
            "command": "bun run lint",
            "icon": "lint",
            "runOnWorktreeCreate": false
        }]);
        let events = decide(
            &env,
            meta_update("cmd-project-update-scripts", "project-scripts", json!({"scripts": scripts})),
            &model,
        )
        .unwrap();
        assert_eq!(events[0]["type"], "project.meta-updated");
        assert_eq!(events[0]["payload"]["scripts"], scripts);
    }

    #[test]
    fn rejects_a_new_script_id_that_cannot_have_a_shortcut() {
        let too_long = "a".repeat(25);
        for id in ["install-javascript-dependencies", "A", "a.b", "a b", "-a", too_long.as_str()] {
            let env = TestEnv::at(EPOCH);
            let model = project_with_scripts(json!([]));
            let failure = decide(
                &env,
                meta_update("cmd-invalid-script", "project-scripts", json!({"scripts": [script("lint"), script(id)]})),
                &model,
            )
            .unwrap_err();
            assert_eq!(tag(&failure), "OrchestrationCommandInvariantError", "{id}");
            let message = failure.to_string();
            assert!(message.contains("Script ID"), "{id}: {message}");
            assert!(message.contains("24"), "{id}: {message}");
            assert_eq!(to_json(&model.projects[0].scripts), json!([]));
        }
    }

    #[test]
    fn accepts_a_script_id_at_the_shortcut_length_limit() {
        let env = TestEnv::at(EPOCH);
        let model = project_with_scripts(json!([]));
        let scripts = json!([script(&"a".repeat(24))]);
        let events = decide(&env, meta_update("cmd-valid-script", "project-scripts", json!({"scripts": scripts})), &model).unwrap();
        assert_matches(&events[0]["payload"], &json!({"scripts": scripts}));
    }

    #[test]
    fn keeps_legacy_scripts_readable_editable_and_removable_while_allowing_valid_additions() {
        let env = TestEnv::at(EPOCH);
        let legacy = script("install-javascript-dependencies");
        let model = project_with_scripts(json!([legacy]));
        assert_eq!(to_json(&model.projects[0].scripts), json!([legacy]));
        let mut edited = legacy.clone();
        edited["command"] = json!("vp install");
        for scripts in [json!([edited, script("lint")]), json!([])] {
            let events = decide(&env, meta_update("cmd-repair-script", "project-scripts", json!({"scripts": scripts})), &model).unwrap();
            assert_matches(&events[0]["payload"], &json!({"scripts": scripts}));
        }
        let failure = decide(
            &env,
            meta_update(
                "cmd-new-invalid-script",
                "project-scripts",
                json!({"scripts": [legacy, script("another.invalid.id")]}),
            ),
            &model,
        )
        .unwrap_err();
        assert_eq!(tag(&failure), "OrchestrationCommandInvariantError");
    }

    #[test]
    fn propagates_project_icon_metadata_in_project_meta_update() {
        let env = TestEnv::at(EPOCH);
        let mut model = empty_model(NOW);
        apply(&mut model, project_created(1, "project-favicon", "Favicon", "/tmp/favicon", json!([])));

        let events = decide(
            &env,
            meta_update(
                "cmd-project-update-favicon",
                "project-favicon",
                json!({
                    "faviconPath": "brand/icon.svg",
                    "projectIcon": {"kind": "lucide", "name": "alarm-clock", "color": "violet"}
                }),
            ),
            &model,
        )
        .unwrap();
        assert_eq!(events[0]["type"], "project.meta-updated");
        assert_eq!(events[0]["payload"]["faviconPath"], "brand/icon.svg");
        assert_eq!(
            events[0]["payload"]["projectIcon"],
            json!({"kind": "lucide", "name": "alarm-clock", "color": "violet"})
        );

        for text in ["T3", "e\u{0301}", "किखि", "क्ष्म", "\u{1100}\u{1161}\u{11a8}"] {
            let monogram = json!({"kind": "monogram", "text": text, "color": "violet"});
            let events = decide(&env, meta_update("cmd-monogram", "project-favicon", json!({"projectIcon": monogram})), &model)
                .unwrap_or_else(|error| panic!("{text}: {error}"));
            // TS plans the decoded icon and the event store encodes it on append; Rust carries
            // icons in their encoded (wire) form throughout, so the planned payload already holds
            // what TS stores: a monogram travels as a lucide icon with `monogramText`.
            assert_matches(
                &events[0]["payload"],
                &json!({"projectIcon": {"kind": "lucide", "name": "folder-code", "color": "violet", "monogramText": text}}),
            );
            assert!(events[0]["payload"]["projectIcon"].get("text").is_none(), "{monogram}");
        }
        for text in ["ABC", "किखिगि"] {
            let failure = decide(
                &env,
                meta_update(
                    "cmd-monogram-invalid",
                    "project-favicon",
                    json!({"projectIcon": {"kind": "monogram", "text": text, "color": "violet"}}),
                ),
                &model,
            )
            .unwrap_err();
            assert_eq!(tag(&failure), "OrchestrationCommandInvariantError", "{text}");
        }
    }

    #[test]
    fn rejects_project_create_for_an_active_workspace_root_that_already_exists() {
        let env = TestEnv::at(EPOCH);
        let mut model = empty_model(NOW);
        apply(&mut model, project_created(1, "project-existing", "Project", "/tmp/project", json!([])));

        let failure = decide(
            &env,
            json!({
                "type": "project.create",
                "commandId": "cmd-project-create-duplicate-root",
                "projectId": "project-duplicate-root",
                "title": "Duplicate Project",
                "workspaceRoot": "/tmp/project/",
                "createdAt": NOW
            }),
            &model,
        )
        .unwrap_err();
        assert!(
            failure
                .to_string()
                .contains("Active project 'project-existing' already exists for workspace root '/tmp/project'."),
            "{failure}"
        );
    }

    #[test]
    fn rejects_project_meta_update_when_moving_onto_another_active_workspace_root() {
        let env = TestEnv::at(EPOCH);
        let mut model = empty_model(NOW);
        apply(&mut model, project_created(1, "project-first", "First", "/tmp/project-first", json!([])));
        apply(&mut model, project_created(2, "project-second", "Second", "/tmp/project-second", json!([])));

        let failure = decide(
            &env,
            meta_update(
                "cmd-project-update-duplicate-root",
                "project-second",
                json!({"workspaceRoot": "/tmp/project-first"}),
            ),
            &model,
        )
        .unwrap_err();
        assert!(
            failure
                .to_string()
                .contains("Active project 'project-first' already exists for workspace root '/tmp/project-first'."),
            "{failure}"
        );
    }

    fn project_and_thread(runtime_mode: &str) -> OrchestrationReadModel {
        let mut model = empty_model(NOW);
        apply(&mut model, project_created(1, "project-1", "Project", "/tmp/project", json!([])));
        apply(
            &mut model,
            make_event(
                2,
                "thread.created",
                NOW,
                "thread-1",
                json!({
                    "threadId": "thread-1",
                    "projectId": "project-1",
                    "title": "Thread",
                    "modelSelection": {"instanceId": "codex", "model": "gpt-5-codex"},
                    "interactionMode": "default",
                    "runtimeMode": runtime_mode,
                    "branch": null,
                    "worktreePath": null,
                    "createdAt": NOW,
                    "updatedAt": NOW
                }),
            ),
        );
        model
    }

    #[test]
    fn emits_user_message_and_turn_start_requested_events_for_thread_turn_start() {
        let env = TestEnv::at(EPOCH);
        let model = project_and_thread("approval-required");
        // `createModelSelection(codex, "gpt-5.3-codex", [...])`.
        let selection = json!({
            "instanceId": "codex",
            "model": "gpt-5.3-codex",
            "options": [
                {"id": "reasoningEffort", "value": "high"},
                {"id": "fastMode", "value": true}
            ]
        });
        let events = decide(
            &env,
            json!({
                "type": "thread.turn.start",
                "commandId": "cmd-turn-start",
                "threadId": "thread-1",
                "message": {"messageId": "message-user-1", "role": "user", "text": "hello", "attachments": []},
                "modelSelection": selection,
                "interactionMode": "default",
                "runtimeMode": "approval-required",
                "createdAt": NOW
            }),
            &model,
        )
        .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "thread.message-sent");
        let turn_start = &events[1];
        assert_eq!(turn_start["type"], "thread.turn-start-requested");
        assert_eq!(turn_start["causationEventId"], events[0]["eventId"]);
        assert_matches(
            &turn_start["payload"],
            &json!({
                "threadId": "thread-1",
                "messageId": "message-user-1",
                "modelSelection": selection,
                "runtimeMode": "approval-required"
            }),
        );
    }

    #[test]
    fn emits_thread_runtime_mode_set_from_thread_runtime_mode_set() {
        let env = TestEnv::at(EPOCH);
        let model = project_and_thread("full-access");
        let events = decide(
            &env,
            json!({
                "type": "thread.runtime-mode.set",
                "commandId": "cmd-runtime-mode-set",
                "threadId": "thread-1",
                "runtimeMode": "approval-required",
                "createdAt": NOW
            }),
            &model,
        )
        .unwrap();
        assert_eq!(events.len(), 1, "Expected a single runtime-mode-set event.");
        assert_matches(
            &events[0],
            &json!({
                "type": "thread.runtime-mode-set",
                "payload": {"threadId": "thread-1", "runtimeMode": "approval-required"}
            }),
        );
    }

    #[test]
    fn emits_thread_interaction_mode_set_from_thread_interaction_mode_set() {
        let env = TestEnv::at(EPOCH);
        let model = project_and_thread("approval-required");
        let events = decide(
            &env,
            json!({
                "type": "thread.interaction-mode.set",
                "commandId": "cmd-interaction-mode-set",
                "threadId": "thread-1",
                "interactionMode": "plan",
                "createdAt": NOW
            }),
            &model,
        )
        .unwrap();
        assert_eq!(events.len(), 1, "Expected a single interaction-mode-set event.");
        assert_matches(
            &events[0],
            &json!({
                "type": "thread.interaction-mode-set",
                "payload": {"threadId": "thread-1", "interactionMode": "plan"}
            }),
        );
    }
}

// ---------------------------------------------------------------------------------------------
mod project_thread_env_mode {
    //! `decider.projectThreadEnvMode.test.ts`.

    use super::*;
    use zc_contracts::OrchestrationReadModel;

    const NOW: &str = "2026-01-01T00:00:00.000Z";
    const PROJECT_ID: &str = "project-env-mode";

    fn seed_project_created(sequence: i64) -> Value {
        let mut event = make_event(
            sequence,
            "project.created",
            NOW,
            PROJECT_ID,
            json!({
                "projectId": PROJECT_ID,
                "title": "Env mode",
                "workspaceRoot": "/tmp/env-mode",
                "defaultModelSelection": null,
                "scripts": [],
                "createdAt": NOW,
                "updatedAt": NOW
            }),
        );
        event["eventId"] = json!(format!("evt-project-env-mode-{sequence}"));
        event["commandId"] = json!(format!("cmd-project-env-mode-{sequence}"));
        event["correlationId"] = json!(format!("cmd-project-env-mode-{sequence}"));
        event
    }

    fn seeded() -> OrchestrationReadModel {
        let mut model = empty_model(NOW);
        apply(&mut model, seed_project_created(1));
        model
    }

    fn meta_update(command_id: &str, fields: Value) -> Value {
        let mut command = json!({"type": "project.meta.update", "commandId": command_id, "projectId": PROJECT_ID});
        merge(&mut command, fields);
        command
    }

    /// `projectEvent(model, { ...event, sequence })`, returning the new model.
    fn projected(model: &OrchestrationReadModel, event: &Value, sequence: i64) -> OrchestrationReadModel {
        let mut next = model.clone();
        let mut event = event.clone();
        event["sequence"] = json!(sequence);
        apply(&mut next, event);
        next
    }

    #[test]
    fn only_treats_metadata_updates_as_explicit_model_defaults() {
        let env = TestEnv::at(EPOCH);
        let selection = json!({
            "instanceId": "codex",
            "model": "gpt-5.6-sol",
            "options": [{"id": "reasoningEffort", "value": "high"}]
        });
        let created = decide(
            &env,
            json!({
                "type": "project.create",
                "commandId": "cmd-project-model-create",
                "projectId": PROJECT_ID,
                "title": "Model default",
                "workspaceRoot": "/tmp/model-default",
                "defaultModelSelection": selection,
                "createdAt": NOW
            }),
            &empty_model(NOW),
        )
        .unwrap();
        let created_event = &created[0];
        assert_eq!(created_event["type"], "project.created");
        assert_eq!(created_event["payload"].get("defaultModelSelection"), Some(&Value::Null));

        let with_project = projected(&empty_model(NOW), created_event, 1);
        let updated = decide(
            &env,
            meta_update("cmd-project-model-update", json!({"defaultModelSelection": selection})),
            &with_project,
        )
        .unwrap();
        let updated_event = &updated[0];
        assert_eq!(updated_event["type"], "project.meta-updated");
        assert_eq!(updated_event["payload"]["defaultModelSelection"], selection);
    }

    #[test]
    fn propagates_default_thread_env_mode_through_meta_update_into_the_read_model() {
        let env = TestEnv::at(EPOCH);
        let model = seeded();
        assert_eq!(to_json(&model.projects[0]).get("defaultThreadEnvMode"), Some(&Value::Null));

        let events = decide(
            &env,
            meta_update("cmd-project-env-mode-set", json!({"defaultThreadEnvMode": "worktree"})),
            &model,
        )
        .unwrap();
        let event = &events[0];
        assert_eq!(event["type"], "project.meta-updated");
        assert_eq!(event["payload"]["defaultThreadEnvMode"], "worktree");

        let updated = projected(&model, event, 2);
        assert_eq!(to_json(&updated.projects[0])["defaultThreadEnvMode"], "worktree");
    }

    #[test]
    fn omits_the_field_when_unset_and_clears_it_on_explicit_null() {
        let env = TestEnv::at(EPOCH);
        let model = seeded();

        let unrelated = decide(&env, meta_update("cmd-project-env-mode-title", json!({"title": "Renamed"})), &model).unwrap();
        assert!(unrelated[0]["payload"].get("defaultThreadEnvMode").is_none(), "{}", unrelated[0]["payload"]);

        let set = decide(
            &env,
            meta_update("cmd-project-env-mode-set", json!({"defaultThreadEnvMode": "worktree"})),
            &model,
        )
        .unwrap();
        let after_set = projected(&model, &set[0], 2);

        let clear = decide(
            &env,
            meta_update("cmd-project-env-mode-clear", json!({"defaultThreadEnvMode": null})),
            &after_set,
        )
        .unwrap();
        let after_clear = projected(&after_set, &clear[0], 3);
        assert_eq!(to_json(&after_clear.projects[0]).get("defaultThreadEnvMode"), Some(&Value::Null));
    }

    #[test]
    fn propagates_auto_pull_through_meta_update_into_the_read_model() {
        let env = TestEnv::at(EPOCH);
        let model = seeded();
        assert_eq!(to_json(&model.projects[0]).get("autoPull"), Some(&json!(false)));

        let events = decide(&env, meta_update("cmd-project-auto-pull", json!({"autoPull": true})), &model).unwrap();
        assert_eq!(events[0]["payload"]["autoPull"], true);

        let updated = projected(&model, &events[0], 2);
        assert_eq!(to_json(&updated.projects[0])["autoPull"], true);
    }
}
