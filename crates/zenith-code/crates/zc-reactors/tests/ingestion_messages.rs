//! Port of `ProviderRuntimeIngestion.test.ts`: assistant and reasoning messages, tool
//! activities, proposed plans.

mod common;

use common::*;
use serde_json::{json, Value};

fn messages(thread: &Value) -> &Value {
    &thread["messages"]
}

fn with_role<'a>(thread: &'a Value, role: &str) -> Vec<&'a Value> {
    filter(messages(thread), |message| s(message, "role") == role)
}

fn finished(message: &Value) -> bool {
    message["streaming"] == json!(false)
}

#[tokio::test]
async fn maps_canonical_content_delta_item_completed_into_finalized_assistant_messages() {
    let h = IngestionHarness::new(Default::default()).await;
    for (id, delta) in [("evt-message-delta-1", "hello"), ("evt-message-delta-2", " world")] {
        h.emit(json!({"type": "content.delta", "eventId": id, "turnId": "turn-2", "itemId": "item-1", "payload": {"streamKind": "assistant_text", "delta": delta}}));
    }
    h.emit(json!({"type": "item.completed", "eventId": "evt-message-completed", "turnId": "turn-2", "itemId": "item-1", "payload": {"itemType": "assistant_message", "status": "completed"}}));
    let thread = h
        .wait_for_thread(|t| find(messages(t), |m| s(m, "id") == "assistant:item-1" && finished(m)).is_some())
        .await;
    let message = find(messages(&thread), |m| s(m, "id") == "assistant:item-1").unwrap();
    assert_eq!(message["text"], "hello world");
    assert_eq!(message["streaming"], false);
}

#[tokio::test]
async fn streams_reasoning_deltas_into_a_finalized_reasoning_message() {
    let h = IngestionHarness::new(Default::default()).await;
    for delta in ["Weighing ", "the options"] {
        h.emit(json!({
            "type": "content.delta", "eventId": format!("evt-reasoning-{}", delta.trim()), "turnId": "turn-reasoning", "itemId": "item-r1",
            "payload": {"streamKind": "reasoning_text", "delta": delta},
        }));
    }
    h.emit(json!({"type": "item.completed", "eventId": "evt-reasoning-completed", "turnId": "turn-reasoning", "itemId": "item-r1", "payload": {"itemType": "reasoning", "status": "completed"}}));
    let thread = h
        .wait_for_thread(|t| with_role(t, "reasoning").iter().any(|m| finished(m) && m["text"] == "Weighing the options"))
        .await;
    assert_eq!(with_role(&thread, "reasoning")[0]["text"], "Weighing the options");
    assert!(with_role(&thread, "assistant").is_empty());
}

#[tokio::test]
async fn keeps_a_reasoning_summarys_parts_apart() {
    let h = IngestionHarness::new(Default::default()).await;
    for (index, delta) in [(0, "**First**"), (1, "**Second**")] {
        h.emit(json!({
            "type": "content.delta", "eventId": format!("evt-summary-{index}"), "turnId": "turn-summary", "itemId": "item-s1",
            "payload": {"streamKind": "reasoning_summary_text", "delta": delta, "summaryIndex": index},
        }));
    }
    h.emit(json!({"type": "item.completed", "eventId": "evt-summary-completed", "turnId": "turn-summary", "itemId": "item-s1", "payload": {"itemType": "reasoning", "status": "completed"}}));
    let thread = h
        .wait_for_thread(|t| with_role(t, "reasoning").iter().any(|m| finished(m) && m["text"] == "**First**\n\n**Second**"))
        .await;
    assert_eq!(with_role(&thread, "reasoning")[0]["text"], "**First**\n\n**Second**");
}

#[tokio::test]
async fn uses_a_reasoning_items_detail_when_no_reasoning_deltas_were_streamed() {
    let h = IngestionHarness::new(Default::default()).await;
    let snapshot = json!({
        "type": "item.completed", "eventId": "evt-reasoning-snapshot", "turnId": "turn-snapshot", "itemId": "item-snapshot",
        "payload": {"itemType": "reasoning", "status": "completed", "detail": "reasoning reported in one piece"},
    });
    h.emit(snapshot.clone());
    let thread = h
        .wait_for_thread(|t| {
            with_role(t, "reasoning")
                .iter()
                .any(|m| finished(m) && m["text"] == "reasoning reported in one piece")
        })
        .await;
    assert_eq!(with_role(&thread, "reasoning")[0]["text"], "reasoning reported in one piece");
    for (index, detail) in ["", " \n\t"].iter().enumerate() {
        h.emit(json!({
            "type": "item.completed", "eventId": format!("evt-empty-reasoning-{index}"), "turnId": format!("turn-empty-{index}"),
            "itemId": format!("item-empty-{index}"), "payload": {"itemType": "reasoning", "status": "completed", "detail": detail},
        }));
    }
    // A repeated completion must rewrite that row, not add a second copy.
    let mut repeat = snapshot;
    repeat["eventId"] = json!("evt-reasoning-snapshot-repeat");
    h.emit(repeat);
    h.drain().await;
    assert_eq!(with_role(&h.thread().await, "reasoning").len(), 1);
}

#[tokio::test]
async fn keeps_interleaved_summary_and_raw_reasoning_in_separate_blocks() {
    let h = IngestionHarness::new(Default::default()).await;
    for (tag, at, kind, delta) in [
        ("a", "2026-01-01T00:00:01.000Z", "reasoning_summary_text", "summary one"),
        ("b", "2026-01-01T00:00:02.000Z", "reasoning_text", "raw one"),
        ("c", "2026-01-01T00:00:03.000Z", "reasoning_summary_text", "summary two"),
    ] {
        h.emit(json!({
            "type": "content.delta", "eventId": format!("evt-interleaved-{tag}"), "createdAt": at, "turnId": "turn-interleaved",
            "itemId": "item-interleaved", "payload": {"streamKind": kind, "delta": delta},
        }));
    }
    h.emit(json!({"type": "item.completed", "eventId": "evt-interleaved-completed", "turnId": "turn-interleaved", "itemId": "item-interleaved", "payload": {"itemType": "reasoning", "status": "completed"}}));
    let thread = h
        .wait_for_thread(|t| with_role(t, "reasoning").iter().filter(|m| finished(m)).count() == 3)
        .await;
    let reasoning = with_role(&thread, "reasoning");
    let texts: Vec<&str> = reasoning.iter().map(|m| s(m, "text")).collect();
    assert_eq!(texts, ["summary one", "raw one", "summary two"]);
    let ids: std::collections::HashSet<&str> = reasoning.iter().map(|m| s(m, "id")).collect();
    assert_eq!(ids.len(), 3);
}

async fn closes_reasoning_when_the_assistant_answers(with_deltas: bool) {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit(json!({"type": "content.delta", "eventId": "evt-think-before-answer", "provider": "claude", "turnId": "turn-answer", "payload": {"streamKind": "reasoning_text", "delta": "thinking it through"}}));
    if with_deltas {
        h.emit(json!({"type": "content.delta", "eventId": "evt-answer-after-think", "provider": "claude", "turnId": "turn-answer", "itemId": "item-a1", "payload": {"streamKind": "assistant_text", "delta": "the answer"}}));
    }
    let mut payload = json!({"itemType": "assistant_message", "status": "completed"});
    if !with_deltas {
        payload["detail"] = json!("the answer");
    }
    h.emit(json!({"type": "item.completed", "eventId": "evt-answer-completed", "provider": "claude", "turnId": "turn-answer", "itemId": "item-a1", "payload": payload}));
    let thread = h
        .wait_for_thread(|t| with_role(t, "reasoning").iter().any(|m| finished(m)) && with_role(t, "assistant").iter().any(|m| finished(m)))
        .await;
    let reasoning = with_role(&thread, "reasoning")[0];
    assert_eq!(reasoning["text"], "thinking it through");
    assert_eq!(reasoning["streaming"], false);
    assert_eq!(with_role(&thread, "assistant")[0]["text"], "the answer");
}

#[tokio::test]
async fn closes_reasoning_when_the_assistant_answers_with_deltas() {
    closes_reasoning_when_the_assistant_answers(true).await;
}

#[tokio::test]
async fn closes_reasoning_when_the_assistant_answers_without_deltas() {
    closes_reasoning_when_the_assistant_answers(false).await;
}

#[tokio::test]
async fn starts_a_new_reasoning_block_after_tool_work() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit(json!({"type": "content.delta", "eventId": "evt-think-before-tool", "provider": "claude", "turnId": "turn-tooled", "payload": {"streamKind": "reasoning_text", "delta": "before the tool"}}));
    h.emit(json!({
        "type": "item.started", "eventId": "evt-tool-between-thoughts", "provider": "claude", "turnId": "turn-tooled", "itemId": "item-tool-1",
        "payload": {"itemType": "command_execution", "status": "inProgress", "title": "ls"},
    }));
    h.emit(json!({"type": "content.delta", "eventId": "evt-think-after-tool", "provider": "claude", "turnId": "turn-tooled", "payload": {"streamKind": "reasoning_text", "delta": "after the tool"}}));
    h.emit(json!({"type": "item.completed", "eventId": "evt-second-thought-completed", "provider": "claude", "turnId": "turn-tooled", "payload": {"itemType": "reasoning", "status": "completed"}}));
    let thread = h
        .wait_for_thread(|t| with_role(t, "reasoning").iter().filter(|m| finished(m)).count() == 2)
        .await;
    let reasoning = with_role(&thread, "reasoning");
    let texts: Vec<&str> = reasoning.iter().map(|m| s(m, "text")).collect();
    assert_eq!(texts, ["before the tool", "after the tool"]);
    assert_eq!(reasoning[0]["streaming"], false);
}

#[tokio::test]
async fn uses_assistant_item_completion_detail_when_no_assistant_deltas_were_streamed() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit(json!({
        "type": "item.completed", "eventId": "evt-assistant-item-completed-no-delta", "turnId": "turn-no-delta", "itemId": "item-no-delta",
        "payload": {"itemType": "assistant_message", "status": "completed", "detail": "assistant-only final text"},
    }));
    let thread = h
        .wait_for_thread(|t| find(messages(t), |m| s(m, "id") == "assistant:item-no-delta" && finished(m)).is_some())
        .await;
    let message = find(messages(&thread), |m| s(m, "id") == "assistant:item-no-delta").unwrap();
    assert_eq!(message["text"], "assistant-only final text");
}

async fn activity(h: &IngestionHarness, id: &str) -> Value {
    let thread = h.wait_for_thread(|t| find(&t["activities"], |a| s(a, "id") == id).is_some()).await;
    find(&thread["activities"], |a| s(a, "id") == id).unwrap().clone()
}

#[tokio::test]
async fn preserves_completed_tool_metadata_on_projected_tool_activities() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit(json!({
        "type": "item.completed", "eventId": "evt-tool-completed-with-data", "provider": "cursor", "turnId": "turn-tool-completed",
        "itemId": "item-tool-completed",
        "payload": {
            "itemType": "dynamic_tool_call", "status": "completed", "title": "Read file",
            "data": {"toolCallId": "tool-read-1", "kind": "read", "rawOutput": {"content": "import * as Effect from \"effect/Effect\"\n"}},
        },
    }));
    let activity = activity(&h, "evt-tool-completed-with-data").await;
    assert_eq!(activity["kind"], "tool.completed");
    assert_eq!(activity["summary"], "Read file");
    assert_eq!(activity["payload"]["itemType"], "dynamic_tool_call");
    assert!(activity["payload"].get("detail").is_none());
    assert_eq!(activity["payload"]["data"]["toolCallId"], "tool-read-1");
    assert_eq!(activity["payload"]["data"]["kind"], "read");
    assert_eq!(
        activity["payload"]["data"]["rawOutput"]["content"],
        "import * as Effect from \"effect/Effect\"\n"
    );
}

#[tokio::test]
async fn normalizes_command_execution_activities_to_ran_command_summaries() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit(json!({
        "type": "item.completed", "eventId": "evt-command-completed", "provider": "cursor", "turnId": "turn-command-completed",
        "itemId": "item-command-completed",
        "payload": {
            "itemType": "command_execution", "status": "completed", "title": "Ran command", "detail": "bun run lint",
            "data": {"toolCallId": "tool-command-1", "kind": "execute", "command": "bun run lint"},
        },
    }));
    let activity = activity(&h, "evt-command-completed").await;
    assert_eq!(activity["summary"], "Ran command");
    assert_eq!(activity["payload"]["detail"], "bun run lint");
}

#[tokio::test]
async fn uses_structured_read_file_paths_when_available() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit(json!({
        "type": "item.completed", "eventId": "evt-read-path-completed", "provider": "cursor", "turnId": "turn-read-path",
        "itemId": "item-read-path",
        "payload": {
            "itemType": "dynamic_tool_call", "status": "completed", "title": "Read file", "detail": "/tmp/app.ts",
            "data": {"toolCallId": "tool-read-path-1", "kind": "read", "locations": [{"path": "/tmp/app.ts"}]},
        },
    }));
    let activity = activity(&h, "evt-read-path-completed").await;
    assert_eq!(activity["summary"], "Read file");
    assert_eq!(activity["payload"]["detail"], "/tmp/app.ts");
}

#[tokio::test]
async fn projects_completed_plan_items_into_first_class_proposed_plans() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit(json!({
        "type": "turn.proposed.completed", "eventId": "evt-plan-item-completed", "turnId": "turn-plan-final",
        "payload": {"planMarkdown": "## Ship plan\n\n- wire projection\n- render follow-up"},
    }));
    let thread = h
        .wait_for_thread(|t| find(&t["proposedPlans"], |p| s(p, "id") == "plan:thread-1:turn:turn-plan-final").is_some())
        .await;
    let plan = find(&thread["proposedPlans"], |p| s(p, "id") == "plan:thread-1:turn:turn-plan-final").unwrap();
    assert_eq!(plan["planMarkdown"], "## Ship plan\n\n- wire projection\n- render follow-up");
}
