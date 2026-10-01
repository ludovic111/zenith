//! Basic decider flows (create a project and a thread, start a turn), checking the event
//! shapes against what `decider.ts` emits.

mod common;

use common::*;
use serde_json::json;

const NOW: &str = "2026-01-01T00:00:00.000Z";

#[test]
fn creates_a_project_then_a_thread_then_starts_a_turn() {
    let env = TestEnv::at(NOW);
    let mut model = empty_model(NOW);

    let events = decide(
        &env,
        json!({
            "type": "project.create",
            "commandId": "cmd-project",
            "projectId": "project-1",
            "title": "Project",
            "workspaceRoot": "/tmp/project-1",
            "createdAt": NOW
        }),
        &model,
    )
    .unwrap();
    assert_eq!(
        events,
        vec![json!({
            "sequence": 0,
            "eventId": "event-1",
            "aggregateKind": "project",
            "aggregateId": "project-1",
            "occurredAt": NOW,
            "commandId": "cmd-project",
            "causationEventId": null,
            "correlationId": "cmd-project",
            "metadata": {},
            "type": "project.created",
            "payload": {
                "projectId": "project-1",
                "title": "Project",
                "workspaceRoot": "/tmp/project-1",
                "defaultModelSelection": null,
                "faviconPath": null,
                "projectIcon": null,
                "scripts": [],
                "createdAt": NOW,
                "updatedAt": NOW
            }
        })]
    );
    apply_decided(&mut model, &events);

    let events = decide(
        &env,
        json!({
            "type": "thread.create",
            "commandId": "cmd-thread",
            "threadId": "thread-1",
            "projectId": "project-1",
            "title": "Thread",
            "modelSelection": {"instanceId": "codex", "model": "gpt-5.4"},
            "runtimeMode": "full-access",
            "interactionMode": "default",
            "branch": null,
            "worktreePath": null,
            "createdAt": NOW
        }),
        &model,
    )
    .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["type"], "thread.created");
    assert_eq!(events[0]["aggregateKind"], "thread");
    apply_decided(&mut model, &events);
    assert_eq!(
        to_json(&model.threads[0]),
        thread_json(
            "thread-1",
            "project-1",
            NOW,
            json!({
                "branchPullRequest": null,
                "unsettledAt": null,
                "snoozedUntil": null,
                "snoozedAt": null,
                "activeOrderKey": null,
                "autoSettleDisabledAt": null
            })
        )
    );

    let events = decide(
        &env,
        json!({
            "type": "thread.turn.start",
            "commandId": "cmd-turn",
            "threadId": "thread-1",
            "message": {"messageId": "message-1", "role": "user", "text": "hello", "attachments": []},
            "runtimeMode": "full-access",
            "interactionMode": "default",
            "createdAt": NOW
        }),
        &model,
    )
    .unwrap();
    let types: Vec<&str> = events.iter().map(|event| event["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["thread.message-sent", "thread.turn-start-requested"]);
    assert_eq!(events[1]["causationEventId"], events[0]["eventId"]);
    assert_eq!(events[0]["payload"]["attachments"], json!([]));
    assert_eq!(events[0]["payload"]["turnId"], json!(null));
    assert_eq!(events[1]["payload"]["runtimeMode"], "full-access");

    let rejected = decide(&env, json!({"type": "thread.delete", "commandId": "cmd-x", "threadId": "missing"}), &model).unwrap_err();
    assert_eq!(tag(&rejected), "OrchestrationCommandInvariantError");
    assert_eq!(
        rejected.to_string(),
        "Orchestration command invariant failed (thread.delete): Thread 'missing' does not exist for command 'thread.delete'."
    );
}
