//! Ports of the projector unit tests (`orchestration/projector*.test.ts`), one module per TS
//! file. Events are built as wire JSON (like the TS literals) and the projected model is
//! compared through its wire JSON.
//!
//! The Rust projector updates the model in place and cannot fail: where the TS projector fails
//! with `OrchestrationProjectorDecodeError`, Rust fails earlier, when the event JSON is decoded
//! into an `OrchestrationEvent`. Those cases are ported as "the event JSON does not decode".
//!
//! The TS tests sometimes use the legacy `modelSelection: {provider, model}` shape; the ports
//! use the canonical `{instanceId, model}`. Repository names are made up.

mod common;

use common::*;
use serde_json::{json, Value};
use zc_contracts::{OrchestrationEvent, OrchestrationReadModel, OrchestrationThread};

/// The `thread.created` payload most TS tests use.
fn thread_created_payload(thread_id: &str, title: &str, model: &str, now: &str) -> Value {
    json!({
        "threadId": thread_id,
        "projectId": "project-1",
        "title": title,
        "modelSelection": {"instanceId": "codex", "model": model},
        "runtimeMode": "full-access",
        "interactionMode": "default",
        "branch": null,
        "worktreePath": null,
        "createdAt": now,
        "updatedAt": now,
    })
}

fn thread(model: &OrchestrationReadModel) -> &OrchestrationThread {
    model.threads.first().expect("a thread")
}

/// The wire JSON of a field of the first thread (`Value::Null` when absent).
fn field(model: &OrchestrationReadModel, key: &str) -> Value {
    to_json(thread(model)).get(key).cloned().unwrap_or(Value::Null)
}

/// `assert_eq!` on JSON values with a readable diff.
fn assert_json(actual: Value, expected: Value) {
    let diff = json_diff(&expected, &actual, 20);
    assert!(diff.is_empty(), "JSON differs:\n{}\nactual: {actual}", diff.join("\n"));
}

mod projector {
    //! `projector.test.ts`.
    use super::*;

    #[test]
    fn applies_thread_created_events() {
        let now = "2026-01-01T00:00:00.000Z";
        let mut model = empty_model(now);
        // The TS payload omits `interactionMode`; it decodes to "default".
        apply(
            &mut model,
            make_event(
                1,
                "thread.created",
                now,
                "thread-1",
                json!({
                    "threadId": "thread-1",
                    "projectId": "project-1",
                    "title": "demo",
                    "modelSelection": {"instanceId": "codex", "model": "gpt-5-codex"},
                    "runtimeMode": "full-access",
                    "branch": null,
                    "worktreePath": null,
                    "createdAt": now,
                    "updatedAt": now,
                }),
            ),
        );

        assert_eq!(model.snapshot_sequence, 1);
        assert_json(
            to_json(&model.threads),
            json!([{
                "id": "thread-1",
                "projectId": "project-1",
                "title": "demo",
                "modelSelection": {"instanceId": "codex", "model": "gpt-5-codex"},
                "runtimeMode": "full-access",
                "interactionMode": "default",
                "branch": null,
                "worktreePath": null,
                "pullRequests": [],
                "branchPullRequest": null,
                "latestTurn": null,
                "createdAt": now,
                "updatedAt": now,
                "archivedAt": null,
                "activeOrderKey": null,
                "autoSettleDisabledAt": null,
                "settledOverride": null,
                "settledAt": null,
                "unsettledAt": null,
                "snoozedUntil": null,
                "snoozedAt": null,
                "deletedAt": null,
                "messages": [],
                "proposedPlans": [],
                "activities": [],
                "checkpoints": [],
                "session": null,
            }]),
        );
    }

    #[test]
    fn sets_and_clears_branch_pull_requests_without_changing_manual_links() {
        let now = "2026-01-01T00:00:00.000Z";
        let mut model = read_model(json!({
            "snapshotSequence": 0,
            "updatedAt": now,
            "threads": [],
            "projects": [project_json("project-1", "/repo", now, json!({
                "title": "Widgets",
                "repositoryIdentity": {
                    "canonicalKey": "github.com/acme/widgets",
                    "provider": "github",
                    "displayName": "acme/widgets",
                    "locator": {
                        "source": "git-remote",
                        "remoteName": "origin",
                        "remoteUrl": "https://github.com/acme/widgets.git",
                    },
                },
            }))],
        }));
        apply(
            &mut model,
            make_event(
                1,
                "thread.created",
                now,
                "thread-1",
                json!({
                    "threadId": "thread-1",
                    "projectId": "project-1",
                    "title": "Pull request thread",
                    "modelSelection": {"instanceId": "codex", "model": "gpt-5-codex"},
                    "runtimeMode": "full-access",
                    "branch": "feature",
                    "worktreePath": null,
                    "createdAt": now,
                    "updatedAt": now,
                }),
            ),
        );
        let linked_pull_request = json!({
            "projectId": "project-1",
            "repository": "acme/widgets",
            "number": 42,
            "url": "https://github.com/acme/widgets/pull/42",
        });
        let branch_pull_request = json!({
            "projectId": "project-1",
            "repository": "acme/widgets",
            "number": 43,
            "url": "https://github.com/acme/widgets/pull/43",
        });
        let updates = [
            (
                json!({"linkedPullRequest": linked_pull_request, "branchPullRequest": branch_pull_request}),
                branch_pull_request.clone(),
            ),
            (json!({"title": "Renamed thread"}), branch_pull_request.clone()),
            (json!({"branchPullRequest": null}), Value::Null),
        ];

        for (index, (patch, expected)) in updates.into_iter().enumerate() {
            let mut payload = json!({"threadId": "thread-1", "updatedAt": now});
            merge(&mut payload, patch);
            apply(&mut model, make_event(index as i64 + 2, "thread.meta-updated", now, "thread-1", payload));
            assert_json(field(&model, "branchPullRequest"), expected);
            assert_json(field(&model, "linkedPullRequest"), linked_pull_request.clone());
        }
    }

    /// TS: "fails when event payload cannot be decoded by runtime schema". The Rust projector
    /// takes decoded events, so the failure is the event JSON not decoding.
    #[test]
    fn fails_when_event_payload_cannot_be_decoded_by_runtime_schema() {
        let now = "2026-01-01T00:00:00.000Z";
        let event = make_event(
            1,
            "thread.created",
            now,
            "thread-1",
            json!({
                // missing required threadId
                "projectId": "project-1",
                "title": "demo",
                "modelSelection": {"instanceId": "codex", "model": "gpt-5-codex"},
                "branch": null,
                "worktreePath": null,
                "createdAt": now,
                "updatedAt": now,
            }),
        );
        assert!(serde_json::from_value::<OrchestrationEvent>(event).is_err());
    }

    #[test]
    fn applies_thread_archived_and_thread_unarchived_events() {
        let now = "2026-01-01T00:00:00.000Z";
        let later = "2026-01-01T00:00:01.000Z";
        let mut model = empty_model(now);
        apply(
            &mut model,
            make_event(
                1,
                "thread.created",
                now,
                "thread-1",
                thread_created_payload("thread-1", "demo", "gpt-5-codex", now),
            ),
        );

        apply(
            &mut model,
            make_event(
                2,
                "thread.archived",
                later,
                "thread-1",
                json!({"threadId": "thread-1", "archivedAt": later, "updatedAt": later}),
            ),
        );
        assert_eq!(field(&model, "archivedAt"), json!(later));

        apply(
            &mut model,
            make_event(3, "thread.unarchived", later, "thread-1", json!({"threadId": "thread-1", "updatedAt": later})),
        );
        assert_eq!(field(&model, "archivedAt"), Value::Null);
        assert!(thread(&model).archived_at.is_none());
    }

    #[test]
    fn keeps_projector_forward_compatible_for_unhandled_event_types() {
        let now = "2026-01-01T00:00:00.000Z";
        let mut model = empty_model(now);
        apply(
            &mut model,
            make_event(
                7,
                "thread.turn-start-requested",
                "2026-01-01T00:00:00.000Z",
                "thread-1",
                json!({
                    "threadId": "thread-1",
                    "messageId": "message-1",
                    "runtimeMode": "approval-required",
                    "createdAt": "2026-01-01T00:00:00.000Z",
                }),
            ),
        );
        assert_eq!(model.snapshot_sequence, 7);
        assert_eq!(model.updated_at, "2026-01-01T00:00:00.000Z");
        assert!(model.threads.is_empty());
    }

    /// `effectIt.effect.each` "preserves the turn state after a %s session captures its
    /// checkpoint".
    fn preserves_turn_state_after_session_captures_checkpoint(status: &str, state: &str) {
        let created_at = "2026-02-23T08:00:00.000Z";
        let started_at = "2026-02-23T08:00:05.000Z";
        let mut model = empty_model(created_at);
        apply(
            &mut model,
            make_event(
                1,
                "thread.created",
                created_at,
                "thread-1",
                thread_created_payload("thread-1", "demo", "gpt-5.3-codex", created_at),
            ),
        );

        let settled_at = "2026-02-23T08:01:00.000Z";
        let session = |status: &str, active_turn_id: Value, updated_at: &str| {
            json!({
                "threadId": "thread-1",
                "session": {
                    "threadId": "thread-1",
                    "status": status,
                    "providerName": "codex",
                    "providerSessionId": "session-1",
                    "providerThreadId": "provider-thread-1",
                    "runtimeMode": "approval-required",
                    "activeTurnId": active_turn_id,
                    "lastError": null,
                    "updatedAt": updated_at,
                },
            })
        };
        apply(
            &mut model,
            make_event(2, "thread.session-set", started_at, "thread-1", session("running", json!("turn-1"), started_at)),
        );
        let after_running = model.clone();
        apply(
            &mut model,
            make_event(3, "thread.session-set", settled_at, "thread-1", session(status, Value::Null, settled_at)),
        );

        let running = to_json(thread(&after_running));
        assert_eq!(running["latestTurn"]["turnId"], json!("turn-1"));
        assert_eq!(running["session"]["status"], json!("running"));

        // Leaving the "running" session status settles the running turn with the session
        // timestamp as the turn end.
        let settled = to_json(thread(&model));
        assert_eq!(settled["latestTurn"]["turnId"], json!("turn-1"));
        assert_eq!(settled["latestTurn"]["state"], json!(state));
        assert_eq!(settled["latestTurn"]["completedAt"], json!(settled_at));

        apply(
            &mut model,
            make_event(
                4,
                "thread.turn-diff-completed",
                settled_at,
                "thread-1",
                json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "checkpointTurnCount": 1,
                    "checkpointRef": "refs/t3/checkpoints/thread-1/turn/1",
                    "status": "ready",
                    "files": [],
                    "assistantMessageId": "assistant:turn-1",
                    "completedAt": settled_at,
                }),
            ),
        );
        let captured = to_json(thread(&model));
        assert_eq!(captured["latestTurn"]["state"], json!(state));
        assert_eq!(captured["checkpoints"][0]["status"], json!("ready"));
    }

    #[test]
    fn preserves_the_turn_state_after_a_ready_session_captures_its_checkpoint() {
        preserves_turn_state_after_session_captures_checkpoint("ready", "completed");
    }

    #[test]
    fn preserves_the_turn_state_after_an_interrupted_session_captures_its_checkpoint() {
        preserves_turn_state_after_session_captures_checkpoint("interrupted", "interrupted");
    }

    /// `effectIt.effect.each` "replaces a missing checkpoint without inventing interruption
    /// for a %s session".
    fn replaces_missing_checkpoint(session_status: Option<&str>) {
        let now = "2026-09-04T23:00:00.000Z";
        let thread_id = "thread-placeholder";
        let event = |sequence: i64, event_type: &str, payload: Value| make_event(sequence, event_type, now, thread_id, payload);
        let mut model = empty_model(now);
        apply(
            &mut model,
            event(1, "thread.created", thread_created_payload(thread_id, "Placeholder", "test", now)),
        );
        let checkpoint = |status: &str, checkpoint_ref: &str| {
            json!({
                "threadId": thread_id,
                "turnId": "turn-placeholder",
                "checkpointTurnCount": 1,
                "checkpointRef": checkpoint_ref,
                "files": [],
                "assistantMessageId": "assistant:placeholder",
                "completedAt": now,
                "status": status,
            })
        };
        let session = |status: &str, active_turn_id: Value| {
            json!({
                "threadId": thread_id,
                "session": {
                    "threadId": thread_id,
                    "status": status,
                    "providerName": "codex",
                    "runtimeMode": "full-access",
                    "activeTurnId": active_turn_id,
                    "lastError": null,
                    "updatedAt": now,
                },
            })
        };
        let interrupting = matches!(session_status, Some("interrupted" | "stopped"));
        if interrupting {
            apply(&mut model, event(2, "thread.session-set", session("running", json!("turn-placeholder"))));
        }
        apply(
            &mut model,
            event(3, "thread.turn-diff-completed", checkpoint("missing", "provider-diff:placeholder")),
        );
        if let Some(status) = session_status {
            apply(&mut model, event(4, "thread.session-set", session(status, Value::Null)));
        }
        apply(
            &mut model,
            event(
                5,
                "thread.turn-diff-completed",
                checkpoint("ready", "refs/t3/checkpoints/thread-placeholder/turn/1"),
            ),
        );
        assert_eq!(
            field(&model, "latestTurn")["state"],
            json!(if interrupting { "interrupted" } else { "completed" })
        );
    }

    #[test]
    fn replaces_a_missing_checkpoint_without_inventing_interruption_for_a_null_session() {
        replaces_missing_checkpoint(None);
    }

    #[test]
    fn replaces_a_missing_checkpoint_without_inventing_interruption_for_a_ready_session() {
        replaces_missing_checkpoint(Some("ready"));
    }

    #[test]
    fn replaces_a_missing_checkpoint_without_inventing_interruption_for_an_interrupted_session() {
        replaces_missing_checkpoint(Some("interrupted"));
    }

    #[test]
    fn replaces_a_missing_checkpoint_without_inventing_interruption_for_a_stopped_session() {
        replaces_missing_checkpoint(Some("stopped"));
    }

    #[test]
    fn updates_canonical_thread_runtime_mode_from_thread_runtime_mode_set() {
        let created_at = "2026-02-23T08:00:00.000Z";
        let updated_at = "2026-02-23T08:00:05.000Z";
        let mut model = empty_model(created_at);
        apply(
            &mut model,
            make_event(
                1,
                "thread.created",
                created_at,
                "thread-1",
                thread_created_payload("thread-1", "demo", "gpt-5.3-codex", created_at),
            ),
        );
        apply(
            &mut model,
            make_event(
                2,
                "thread.runtime-mode-set",
                updated_at,
                "thread-1",
                json!({"threadId": "thread-1", "runtimeMode": "approval-required", "updatedAt": updated_at}),
            ),
        );
        assert_eq!(field(&model, "runtimeMode"), json!("approval-required"));
        assert_eq!(field(&model, "updatedAt"), json!(updated_at));
    }

    #[test]
    fn marks_assistant_messages_completed_with_non_streaming_updates() {
        let created_at = "2026-02-23T09:00:00.000Z";
        let delta_at = "2026-02-23T09:00:01.000Z";
        let complete_at = "2026-02-23T09:00:03.500Z";
        let mut model = empty_model(created_at);
        apply(
            &mut model,
            make_event(
                1,
                "thread.created",
                created_at,
                "thread-1",
                thread_created_payload("thread-1", "demo", "gpt-5.3-codex", created_at),
            ),
        );
        let message = |text: &str, streaming: bool, at: &str| {
            json!({
                "threadId": "thread-1",
                "messageId": "assistant:msg-1",
                "role": "assistant",
                "text": text,
                "turnId": "turn-1",
                "streaming": streaming,
                "createdAt": at,
                "updatedAt": at,
            })
        };
        apply(
            &mut model,
            make_event(2, "thread.message-sent", delta_at, "thread-1", message("hello", true, delta_at)),
        );
        apply(
            &mut model,
            make_event(3, "thread.message-sent", complete_at, "thread-1", message("", false, complete_at)),
        );

        let message = &field(&model, "messages")[0];
        assert_eq!(message["id"], json!("assistant:msg-1"));
        assert_eq!(message["text"], json!("hello"));
        assert_eq!(message["streaming"], json!(false));
        assert_eq!(message["updatedAt"], json!(complete_at));
    }

    fn message_sent(sequence: i64, thread_id: &str, at: &str, message_id: &str, role: &str, text: &str, turn_id: Value) -> Value {
        make_event(
            sequence,
            "thread.message-sent",
            at,
            thread_id,
            json!({
                "threadId": thread_id,
                "messageId": message_id,
                "role": role,
                "text": text,
                "turnId": turn_id,
                "streaming": false,
                "createdAt": at,
                "updatedAt": at,
            }),
        )
    }

    fn turn_diff_completed(sequence: i64, thread_id: &str, at: &str, turn_id: &str, count: i64, assistant_message_id: &str) -> Value {
        make_event(
            sequence,
            "thread.turn-diff-completed",
            at,
            thread_id,
            json!({
                "threadId": thread_id,
                "turnId": turn_id,
                "checkpointTurnCount": count,
                "checkpointRef": format!("refs/t3/checkpoints/{thread_id}/turn/{count}"),
                "status": "ready",
                "files": [],
                "assistantMessageId": assistant_message_id,
                "completedAt": at,
            }),
        )
    }

    fn tool_activity(sequence: i64, at: &str, id: &str, kind: &str, summary: &str, turn_id: &str) -> Value {
        make_event(
            sequence,
            "thread.activity-appended",
            at,
            "thread-1",
            json!({
                "threadId": "thread-1",
                "activity": {
                    "id": id,
                    "tone": "tool",
                    "kind": kind,
                    "summary": summary,
                    "payload": {"toolKind": "command"},
                    "turnId": turn_id,
                    "createdAt": at,
                },
            }),
        )
    }

    #[test]
    fn prunes_reverted_turn_messages_from_in_memory_thread_snapshot() {
        let created_at = "2026-02-23T10:00:00.000Z";
        let mut model = empty_model(created_at);
        apply(
            &mut model,
            make_event(
                1,
                "thread.created",
                created_at,
                "thread-1",
                thread_created_payload("thread-1", "demo", "gpt-5.3-codex", created_at),
            ),
        );
        let events = [
            message_sent(2, "thread-1", "2026-02-23T10:00:01.000Z", "user-msg-1", "user", "First edit", Value::Null),
            message_sent(
                3,
                "thread-1",
                "2026-02-23T10:00:02.000Z",
                "assistant-msg-1",
                "assistant",
                "Updated README to v2.\n",
                json!("turn-1"),
            ),
            turn_diff_completed(4, "thread-1", "2026-02-23T10:00:02.500Z", "turn-1", 1, "assistant-msg-1"),
            tool_activity(5, "2026-02-23T10:00:02.750Z", "activity-1", "tool.started", "Edit file started", "turn-1"),
            message_sent(6, "thread-1", "2026-02-23T10:00:03.000Z", "user-msg-2", "user", "Second edit", Value::Null),
            message_sent(
                7,
                "thread-1",
                "2026-02-23T10:00:04.000Z",
                "assistant-msg-2",
                "assistant",
                "Updated README to v3.\n",
                json!("turn-2"),
            ),
            turn_diff_completed(8, "thread-1", "2026-02-23T10:00:04.500Z", "turn-2", 2, "assistant-msg-2"),
            tool_activity(9, "2026-02-23T10:00:04.750Z", "activity-2", "tool.completed", "Edit file complete", "turn-2"),
            make_event(
                10,
                "thread.reverted",
                "2026-02-23T10:00:05.000Z",
                "thread-1",
                json!({"threadId": "thread-1", "turnCount": 1}),
            ),
        ];
        for event in events {
            apply(&mut model, event);
        }

        let thread = to_json(thread(&model));
        let messages: Vec<Value> = thread["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| json!({"role": message["role"], "text": message["text"]}))
            .collect();
        assert_eq!(
            messages,
            vec![
                json!({"role": "user", "text": "First edit"}),
                json!({"role": "assistant", "text": "Updated README to v2.\n"}),
            ]
        );
        let activities: Vec<Value> = thread["activities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|activity| json!({"id": activity["id"], "turnId": activity["turnId"]}))
            .collect();
        assert_eq!(activities, vec![json!({"id": "activity-1", "turnId": "turn-1"})]);
        let counts: Vec<Value> = thread["checkpoints"]
            .as_array()
            .unwrap()
            .iter()
            .map(|checkpoint| checkpoint["checkpointTurnCount"].clone())
            .collect();
        assert_eq!(counts, vec![json!(1)]);
        assert_eq!(thread["latestTurn"]["turnId"], json!("turn-1"));
    }

    #[test]
    fn does_not_fallback_retain_messages_tied_to_removed_turn_ids() {
        let created_at = "2026-02-26T12:00:00.000Z";
        let thread_id = "thread-revert";
        let mut model = empty_model(created_at);
        apply(
            &mut model,
            make_event(
                1,
                "thread.created",
                created_at,
                thread_id,
                thread_created_payload(thread_id, "demo", "gpt-5.3-codex", created_at),
            ),
        );
        let events = [
            turn_diff_completed(2, thread_id, "2026-02-26T12:00:01.000Z", "turn-1", 1, "assistant-keep"),
            message_sent(3, thread_id, "2026-02-26T12:00:01.100Z", "assistant-keep", "assistant", "kept", json!("turn-1")),
            turn_diff_completed(4, thread_id, "2026-02-26T12:00:02.000Z", "turn-2", 2, "assistant-remove"),
            message_sent(5, thread_id, "2026-02-26T12:00:02.050Z", "user-remove", "user", "removed", json!("turn-2")),
            message_sent(
                6,
                thread_id,
                "2026-02-26T12:00:02.100Z",
                "assistant-remove",
                "assistant",
                "removed",
                json!("turn-2"),
            ),
            make_event(
                7,
                "thread.reverted",
                "2026-02-26T12:00:03.000Z",
                thread_id,
                json!({"threadId": thread_id, "turnCount": 1}),
            ),
        ];
        for event in events {
            apply(&mut model, event);
        }

        let messages: Vec<Value> = field(&model, "messages")
            .as_array()
            .unwrap()
            .iter()
            .map(|message| json!({"id": message["id"], "role": message["role"], "turnId": message["turnId"]}))
            .collect();
        assert_eq!(messages, vec![json!({"id": "assistant-keep", "role": "assistant", "turnId": "turn-1"})]);
    }

    #[test]
    fn caps_message_and_checkpoint_retention_for_long_lived_threads() {
        let created_at = "2026-03-01T10:00:00.000Z";
        let thread_id = "thread-capped";
        let mut model = empty_model(created_at);
        apply(
            &mut model,
            make_event(
                1,
                "thread.created",
                created_at,
                thread_id,
                thread_created_payload(thread_id, "capped", "gpt-5-codex", created_at),
            ),
        );
        for index in 0..2_100_i64 {
            let at = format!("2026-03-01T10:00:{:02}.000Z", index % 60);
            apply(
                &mut model,
                message_sent(
                    index + 2,
                    thread_id,
                    &at,
                    &format!("msg-{index}"),
                    "assistant",
                    &format!("message-{index}"),
                    json!(format!("turn-{index}")),
                ),
            );
        }
        for index in 0..600_i64 {
            let at = format!("2026-03-01T10:30:{:02}.000Z", index % 60);
            apply(
                &mut model,
                turn_diff_completed(index + 2_102, thread_id, &at, &format!("turn-{index}"), index + 1, &format!("msg-{index}")),
            );
        }

        let thread = thread(&model);
        assert_eq!(thread.messages.len(), 2_000);
        assert_eq!(thread.messages.first().unwrap().id.as_str(), "msg-100");
        assert_eq!(thread.messages.last().unwrap().id.as_str(), "msg-2099");
        assert_eq!(thread.checkpoints.len(), 500);
        assert_eq!(thread.checkpoints.first().unwrap().turn_id.as_str(), "turn-100");
        assert_eq!(thread.checkpoints.last().unwrap().turn_id.as_str(), "turn-599");
    }

    #[test]
    fn keeps_the_worktree_setup_record_past_the_activity_retention_cap() {
        let created_at = "2026-03-01T10:00:00.000Z";
        let thread_id = "thread-setup-retained";
        let mut model = empty_model(created_at);
        apply(
            &mut model,
            make_event(
                1,
                "thread.created",
                created_at,
                thread_id,
                thread_created_payload(thread_id, "setup retained", "gpt-5-codex", created_at),
            ),
        );
        let activity_event = |sequence: i64, id: &str, kind: &str| {
            let at = format!("2026-03-01T10:{:02}:{:02}.000Z", (sequence / 60) % 60, sequence % 60);
            make_event(
                sequence,
                "thread.activity-appended",
                &at,
                thread_id,
                json!({
                    "threadId": thread_id,
                    "activity": {
                        "id": id,
                        "tone": "info",
                        "kind": kind,
                        "summary": kind,
                        "payload": {},
                        "turnId": null,
                        "createdAt": at,
                    },
                }),
            )
        };
        let setup_id = format!("worktree-setup:{thread_id}");
        apply(&mut model, activity_event(2, &setup_id, "worktree-setup"));
        for index in 0..600_i64 {
            apply(&mut model, activity_event(3 + index, &format!("tool-{index}"), "tool.completed"));
        }
        let thread = model.threads.iter().find(|entry| entry.id.as_str() == thread_id).expect("the thread");
        assert_eq!(thread.activities.len(), 501);
        assert_eq!(thread.activities[0].id.as_str(), setup_id);
    }
}

/// `makeEvent` of the smaller TS files: always on `thread-1`, at a fixed time.
fn thread_event(sequence: i64, event_type: &str, payload: Value) -> Value {
    make_event(sequence, event_type, "2026-01-01T00:00:00.000Z", "thread-1", payload)
}

/// A model holding `thread-1` (the `thread.created` the smaller TS files start with).
fn created_thread_model(now: &str) -> OrchestrationReadModel {
    let mut model = empty_model(now);
    apply(
        &mut model,
        thread_event(1, "thread.created", thread_created_payload("thread-1", "Thread", "gpt-5.4", now)),
    );
    model
}

mod projector_auto_settle_set {
    //! `projector.autoSettleSet.test.ts`.
    use super::*;

    #[test]
    fn projects_auto_settle_opt_out_and_survives_a_manual_settle() {
        let now = "2026-01-01T00:00:00.000Z";
        let later = "2026-01-02T00:00:00.000Z";
        let mut model = created_thread_model(now);
        assert_eq!(field(&model, "autoSettleDisabledAt"), Value::Null);

        apply(
            &mut model,
            thread_event(
                2,
                "thread.auto-settle-set",
                json!({"threadId": "thread-1", "autoSettleDisabledAt": now, "updatedAt": now}),
            ),
        );
        assert_eq!(field(&model, "autoSettleDisabledAt"), json!(now));

        // The flag is independent of the settled lifecycle: settling by hand and un-settling
        // later must not clear it.
        apply(
            &mut model,
            thread_event(3, "thread.settled", json!({"threadId": "thread-1", "settledAt": later, "updatedAt": later})),
        );
        assert_eq!(field(&model, "settledOverride"), json!("settled"));
        assert_eq!(field(&model, "autoSettleDisabledAt"), json!(now));

        apply(
            &mut model,
            thread_event(4, "thread.unsettled", json!({"threadId": "thread-1", "reason": "user", "updatedAt": later})),
        );
        assert_eq!(field(&model, "autoSettleDisabledAt"), json!(now));

        apply(
            &mut model,
            thread_event(
                5,
                "thread.auto-settle-set",
                json!({"threadId": "thread-1", "autoSettleDisabledAt": null, "updatedAt": later}),
            ),
        );
        let thread = to_json(thread(&model));
        assert_eq!(thread.get("autoSettleDisabledAt"), Some(&Value::Null));
    }
}

mod projector_pinned {
    //! `projector.pinned.test.ts`.
    use super::*;

    #[test]
    fn projects_pin_lifecycle_events() {
        let now = "2026-01-01T00:00:00.000Z";
        let mut model = created_thread_model(now);
        assert_eq!(field(&model, "pinnedAt"), Value::Null);

        apply(
            &mut model,
            thread_event(2, "thread.pinned", json!({"threadId": "thread-1", "pinnedAt": now, "updatedAt": now})),
        );
        assert_eq!(field(&model, "pinnedAt"), json!(now));

        apply(
            &mut model,
            thread_event(3, "thread.unpinned", json!({"threadId": "thread-1", "updatedAt": now})),
        );
        // `toBeNull`: present and null.
        assert_eq!(to_json(thread(&model)).get("pinnedAt"), Some(&Value::Null));
    }

    #[test]
    fn projects_pin_order_key_lifecycle() {
        let now = "2026-01-01T00:00:00.000Z";
        let mut model = created_thread_model(now);
        assert_eq!(field(&model, "pinOrderKey"), Value::Null);

        // Fresh pin carries the client's slot in the arranged order.
        apply(
            &mut model,
            thread_event(
                2,
                "thread.pinned",
                json!({"threadId": "thread-1", "pinnedAt": now, "pinOrderKey": "g", "updatedAt": now}),
            ),
        );
        assert_eq!(field(&model, "pinOrderKey"), json!("g"));

        // Re-pins and events from pre-reorder servers omit the field entirely; the existing key
        // must survive rather than being nulled out.
        apply(
            &mut model,
            thread_event(3, "thread.pinned", json!({"threadId": "thread-1", "pinnedAt": now, "updatedAt": now})),
        );
        assert_eq!(field(&model, "pinOrderKey"), json!("g"));

        // A drag persists the new slot.
        apply(
            &mut model,
            thread_event(4, "thread.pin-reordered", json!({"threadId": "thread-1", "orderKey": "m", "updatedAt": now})),
        );
        assert_eq!(field(&model, "pinOrderKey"), json!("m"));

        // Unpin clears the slot: re-pinning is "pin again", not "restore an ancient position".
        apply(
            &mut model,
            thread_event(5, "thread.unpinned", json!({"threadId": "thread-1", "updatedAt": now})),
        );
        assert_eq!(to_json(thread(&model)).get("pinOrderKey"), Some(&Value::Null));
    }
}

mod projector_pull_requests {
    //! `projector.pullRequests.test.ts`.
    use super::*;

    const NOW: &str = "2026-01-01T00:00:00.000Z";
    const LATER: &str = "2026-01-02T00:00:00.000Z";
    const PROJECT_ID: &str = "project-1";

    fn make_link(overrides: Value) -> Value {
        let mut link = json!({
            "host": "github.com",
            "repository": "acme/widgets",
            "number": 42,
            "url": "https://github.com/acme/widgets/pull/42",
            "source": "manual",
            "linkedAt": NOW,
            "snapshot": null,
            "stack": null,
        });
        merge(&mut link, overrides);
        link
    }

    fn snapshot() -> Value {
        json!({
            "state": "merged",
            "title": "Add links",
            "headBranch": "feat/links",
            "baseBranch": "main",
            "isDraft": false,
            "updatedAt": LATER,
            "syncedAt": LATER,
        })
    }

    fn create_thread(model: &mut OrchestrationReadModel) {
        let sequence = model.snapshot_sequence + 1;
        apply(
            model,
            thread_event(sequence, "thread.created", thread_created_payload("thread-1", "Thread", "gpt-5.4", NOW)),
        );
    }

    /// `createProject`: a `project.created`, then the repository identity set on the model
    /// (as the TS helper does).
    fn create_project(model: &mut OrchestrationReadModel, repository_identity: Value) {
        let sequence = model.snapshot_sequence + 1;
        apply(
            model,
            make_event(
                sequence,
                "project.created",
                NOW,
                PROJECT_ID,
                json!({
                    "projectId": PROJECT_ID,
                    "title": "Project",
                    "workspaceRoot": "/workspace/project",
                    "defaultModelSelection": null,
                    "scripts": [],
                    "createdAt": NOW,
                    "updatedAt": NOW,
                }),
            ),
        );
        for project in model.projects.iter_mut().filter(|project| project.id.as_str() == PROJECT_ID) {
            project.repository_identity = Some(Some(decode(repository_identity.clone())));
        }
    }

    fn github_identity(canonical_key: &str, remote_url: &str) -> Value {
        json!({
            "canonicalKey": canonical_key,
            "provider": "github",
            "displayName": "acme/widgets",
            "locator": {"source": "git-remote", "remoteName": "origin", "remoteUrl": remote_url},
        })
    }

    #[test]
    fn seeds_threads_with_no_pull_requests() {
        let mut model = empty_model(NOW);
        create_thread(&mut model);
        assert_eq!(field(&model, "pullRequests"), json!([]));
        assert_eq!(field(&model, "linkedPullRequest"), Value::Null);
    }

    #[test]
    fn projects_link_sync_and_unlink_onto_the_thread() {
        let mut model = empty_model(NOW);
        create_project(&mut model, github_identity("github.com/acme/widgets", "https://github.com/acme/widgets.git"));
        create_thread(&mut model);
        let link = make_link(json!({}));

        apply(
            &mut model,
            thread_event(
                2,
                "thread.pull-request-linked",
                json!({"threadId": "thread-1", "link": link, "updatedAt": LATER}),
            ),
        );
        assert_json(field(&model, "pullRequests"), json!([link]));
        assert_eq!(field(&model, "updatedAt"), json!(LATER));
        // The legacy field is derived from the array so old clients keep working.
        assert_json(
            field(&model, "linkedPullRequest"),
            json!({
                "projectId": PROJECT_ID,
                "repository": "acme/widgets",
                "number": 42,
                "url": "https://github.com/acme/widgets/pull/42",
            }),
        );

        // A second link for the same key replaces in place (used for un-dismiss and stack
        // tombstones), never duplicates.
        let mut relink = link.clone();
        merge(&mut relink, json!({"host": "GITHUB.COM", "source": "agent"}));
        apply(
            &mut model,
            thread_event(
                3,
                "thread.pull-request-linked",
                json!({"threadId": "thread-1", "link": relink, "updatedAt": LATER}),
            ),
        );
        let pull_requests = field(&model, "pullRequests");
        assert_eq!(pull_requests.as_array().unwrap().len(), 1);
        assert_eq!(pull_requests[0]["source"], json!("agent"));

        apply(
            &mut model,
            thread_event(
                4,
                "thread.pull-request-synced",
                json!({
                    "threadId": "thread-1",
                    "host": "github.com",
                    "repository": "acme/widgets",
                    "number": 42,
                    "snapshot": snapshot(),
                    "stack": null,
                    "updatedAt": LATER,
                }),
            ),
        );
        assert_json(field(&model, "pullRequests")[0]["snapshot"].clone(), snapshot());

        apply(
            &mut model,
            thread_event(
                5,
                "thread.pull-request-unlinked",
                json!({
                    "threadId": "thread-1",
                    "host": "github.com",
                    "repository": "acme/widgets",
                    "number": 42,
                    "updatedAt": LATER,
                }),
            ),
        );
        assert_eq!(field(&model, "pullRequests"), json!([]));
        // `toBeNull`: present and null.
        assert_eq!(to_json(thread(&model)).get("linkedPullRequest"), Some(&Value::Null));
    }

    #[test]
    fn ignores_a_sync_for_a_pull_request_that_is_no_longer_linked() {
        let mut model = empty_model(NOW);
        create_thread(&mut model);
        let other = make_link(json!({"number": 7, "url": "https://github.com/acme/widgets/pull/7"}));
        apply(
            &mut model,
            thread_event(
                2,
                "thread.pull-request-linked",
                json!({"threadId": "thread-1", "link": other, "updatedAt": NOW}),
            ),
        );
        apply(
            &mut model,
            thread_event(
                3,
                "thread.pull-request-synced",
                json!({
                    "threadId": "thread-1",
                    "host": "github.com",
                    "repository": "acme/widgets",
                    "number": 42,
                    "snapshot": snapshot(),
                    "stack": null,
                    "updatedAt": LATER,
                }),
            ),
        );
        assert_json(field(&model, "pullRequests"), json!([other]));
        assert_eq!(field(&model, "updatedAt"), json!(NOW));
    }

    #[test]
    fn mirrors_legacy_meta_updated_links_into_pull_requests_using_the_project_host() {
        let mut model = empty_model(NOW);
        create_project(&mut model, github_identity("GitHub.com/acme/widgets", "git@github.com:acme/widgets.git"));
        create_thread(&mut model);
        let agent_link = make_link(json!({
            "number": 7,
            "url": "https://github.com/acme/widgets/pull/7",
            "source": "agent",
        }));
        apply(
            &mut model,
            thread_event(
                3,
                "thread.pull-request-linked",
                json!({"threadId": "thread-1", "link": agent_link, "updatedAt": NOW}),
            ),
        );

        apply(
            &mut model,
            thread_event(
                4,
                "thread.meta-updated",
                json!({
                    "threadId": "thread-1",
                    "linkedPullRequest": {
                        "projectId": PROJECT_ID,
                        "repository": "Acme/Widgets",
                        "number": 42,
                        "url": "https://github.com/acme/widgets/pull/42",
                    },
                    "updatedAt": LATER,
                }),
            ),
        );
        assert_json(
            field(&model, "pullRequests"),
            json!([
                agent_link,
                {
                    "host": "github.com",
                    "repository": "acme/widgets",
                    "number": 42,
                    "url": "https://github.com/acme/widgets/pull/42",
                    "source": "manual",
                    "linkedAt": LATER,
                    "snapshot": null,
                    "stack": null,
                },
            ]),
        );
        // Two open links read as a stack; the derived field points at the top.
        assert_json(
            field(&model, "linkedPullRequest"),
            json!({
                "projectId": PROJECT_ID,
                "repository": "acme/widgets",
                "number": 42,
                "url": "https://github.com/acme/widgets/pull/42",
            }),
        );

        // Null clears only the manual link; the agent's stays.
        apply(
            &mut model,
            thread_event(
                5,
                "thread.meta-updated",
                json!({"threadId": "thread-1", "linkedPullRequest": null, "updatedAt": LATER}),
            ),
        );
        assert_json(field(&model, "pullRequests"), json!([agent_link]));
        assert_json(
            field(&model, "linkedPullRequest"),
            json!({
                "projectId": PROJECT_ID,
                "repository": "acme/widgets",
                "number": 7,
                "url": "https://github.com/acme/widgets/pull/7",
            }),
        );
    }

    #[test]
    fn falls_back_to_the_link_url_host_when_the_project_has_no_repository_identity() {
        let mut model = empty_model(NOW);
        create_thread(&mut model);
        apply(
            &mut model,
            thread_event(
                2,
                "thread.meta-updated",
                json!({
                    "threadId": "thread-1",
                    "linkedPullRequest": {
                        "projectId": PROJECT_ID,
                        "repository": "acme/widgets",
                        "number": 42,
                        "url": "https://GitLab.example.com/acme/widgets/-/merge_requests/42",
                    },
                    "updatedAt": LATER,
                }),
            ),
        );
        assert_eq!(field(&model, "pullRequests")[0]["host"], json!("gitlab.example.com"));
    }

    #[test]
    fn leaves_pull_requests_alone_when_meta_updated_carries_no_legacy_link() {
        let mut model = empty_model(NOW);
        create_thread(&mut model);
        let link = make_link(json!({}));
        apply(
            &mut model,
            thread_event(2, "thread.pull-request-linked", json!({"threadId": "thread-1", "link": link, "updatedAt": NOW})),
        );
        apply(
            &mut model,
            thread_event(
                3,
                "thread.meta-updated",
                json!({"threadId": "thread-1", "title": "Renamed", "updatedAt": LATER}),
            ),
        );
        assert_eq!(field(&model, "title"), json!("Renamed"));
        assert_json(field(&model, "pullRequests"), json!([link]));
    }

    #[test]
    fn replays_azure_legacy_selectors_as_full_repository_keys() {
        let mut model = empty_model(NOW);
        create_project(
            &mut model,
            json!({
                "canonicalKey": "ssh.dev.azure.com/v3/org-a/project/web",
                "provider": "azure-devops",
                "displayName": "v3/org-a/project/web",
                "name": "web",
                "locator": {
                    "source": "git-remote",
                    "remoteName": "origin",
                    "remoteUrl": "git@ssh.dev.azure.com:v3/org-a/project/web",
                },
            }),
        );
        create_thread(&mut model);
        let legacy = json!({
            "projectId": PROJECT_ID,
            "repository": "web",
            "number": 7,
            "url": "https://dev.azure.com/org-a/project/_git/web/pullrequest/7",
        });
        apply(
            &mut model,
            thread_event(
                3,
                "thread.meta-updated",
                json!({"threadId": "thread-1", "linkedPullRequest": legacy, "updatedAt": LATER}),
            ),
        );
        assert_json(
            field(&model, "pullRequests"),
            json!([{
                "host": "dev.azure.com",
                "repository": "org-a/project/_git/web",
                "number": 7,
                "url": legacy["url"],
                "source": "manual",
                "linkedAt": LATER,
                "snapshot": null,
                "stack": null,
            }]),
        );
        assert_json(field(&model, "linkedPullRequest"), legacy);
    }
}

mod projector_settled {
    //! `projector.settled.test.ts`.
    use super::*;

    #[test]
    fn projects_settled_lifecycle_events() {
        let now = "2026-01-01T00:00:00.000Z";
        let mut model = created_thread_model(now);
        let present_null = |model: &OrchestrationReadModel, key: &str| to_json(thread(model)).get(key) == Some(&Value::Null);

        apply(
            &mut model,
            thread_event(2, "thread.settled", json!({"threadId": "thread-1", "settledAt": now, "updatedAt": now})),
        );
        assert_eq!(field(&model, "settledOverride"), json!("settled"));
        assert_eq!(field(&model, "settledAt"), json!(now));
        assert!(present_null(&model, "unsettledAt"));

        let unsettle_at = "2026-01-02T00:00:00.000Z";
        apply(
            &mut model,
            thread_event(
                3,
                "thread.unsettled",
                json!({"threadId": "thread-1", "reason": "user", "updatedAt": unsettle_at}),
            ),
        );
        assert_eq!(field(&model, "settledOverride"), json!("active"));
        assert!(present_null(&model, "settledAt"));
        assert_eq!(field(&model, "unsettledAt"), json!(unsettle_at));

        // Clearing the keep-active pin on activity is not a re-entry: the thread is already in
        // the active list, so the stamp must not move it.
        let activity_at = "2026-01-03T00:00:00.000Z";
        apply(
            &mut model,
            thread_event(
                4,
                "thread.unsettled",
                json!({"threadId": "thread-1", "reason": "activity", "updatedAt": activity_at}),
            ),
        );
        assert!(present_null(&model, "settledOverride"));
        assert!(present_null(&model, "settledAt"));
        assert_eq!(field(&model, "unsettledAt"), json!(unsettle_at));

        let resettled_at = "2026-01-04T00:00:00.000Z";
        apply(
            &mut model,
            thread_event(
                5,
                "thread.settled",
                json!({"threadId": "thread-1", "settledAt": resettled_at, "updatedAt": resettled_at}),
            ),
        );
        assert!(present_null(&model, "unsettledAt"));

        // Waking a settled thread on activity IS a re-entry and stamps.
        let wake_at = "2026-01-05T00:00:00.000Z";
        apply(
            &mut model,
            thread_event(
                6,
                "thread.unsettled",
                json!({"threadId": "thread-1", "reason": "activity", "updatedAt": wake_at}),
            ),
        );
        assert!(present_null(&model, "settledOverride"));
        assert_eq!(field(&model, "unsettledAt"), json!(wake_at));
    }
}
