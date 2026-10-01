// ActivityPayloadProjection.test.ts
use super::*;
use serde_json::json;

fn activity(payload: Value) -> OrchestrationThreadActivity {
    serde_json::from_value(json!({
        "id": "activity-1",
        "tone": "tool",
        "kind": "tool.completed",
        "summary": "Tool",
        "payload": payload,
        "turnId": null,
        "createdAt": "2026-08-01T10:00:00.000Z",
    }))
    .unwrap()
}

fn project(payload: Value) -> Value {
    project_activity_payload(&activity(payload)).payload
}

#[test]
fn preserves_tool_attribution_through_data_slimming() {
    let payload = project(json!({
        "itemType": "command_execution",
        "agentId": "task-123",
        "parentToolUseId": "toolu_abc",
        "data": {
            "toolName": "Bash",
            "input": {"command": "ls"},
            "command": "ls",
            "rawOutput": {"content": "x".repeat(10)},
            "somethingClientNeverReads": {"big": "blob"},
        },
    }));
    assert_eq!(payload["agentId"], "task-123");
    assert_eq!(payload["parentToolUseId"], "toolu_abc");
    assert!(payload["data"].get("somethingClientNeverReads").is_none());
}

#[test]
fn keeps_a_bounded_codex_command_output_summary() {
    let payload = project(json!({
        "itemType": "command_execution",
        "data": {"item": {
            "command": "/bin/zsh -lc 'printf hello'",
            "aggregatedOutput": format!("hello from codex\n{}", "x".repeat(5000)),
        }},
    }));
    assert_eq!(
        payload["data"]["item"],
        json!({"command": "/bin/zsh -lc 'printf hello'", "aggregatedOutput": "hello from codex"})
    );
    assert!(payload.to_string().len() < 500);
}

#[test]
fn keeps_preview_normalization_and_the_fence_only_fallback() {
    let preview = project(json!({
        "itemType": "command_execution",
        "data": {"rawOutput": format!("```\n  actual\tresult  \n{}", "x".repeat(5000))},
    }));
    let fences = project(json!({
        "itemType": "command_execution",
        "data": {"rawOutput": "```\r\n \t \n```\n"},
    }));
    assert_eq!(preview["data"]["rawOutput"], json!({"content": "actual result"}));
    assert_eq!(fences["data"]["rawOutput"], json!({"content": "2 lines"}));
}

#[test]
fn keeps_bounded_claude_and_acp_command_output_summaries() {
    let claude = project(json!({
        "itemType": "command_execution",
        "data": {"command": "printf hello", "rawOutput": {"stdout": format!("hello from claude\n{}", "y".repeat(5000))}},
    }));
    let acp = project(json!({
        "itemType": "command_execution",
        "data": {"command": "printf hello", "content": [
            {"type": "content", "content": {"type": "text", "text": format!("hello from acp\n{}", "z".repeat(5000))}},
        ]},
    }));
    assert_eq!(claude["data"]["rawOutput"], json!({"content": "hello from claude"}));
    assert_eq!(acp["data"]["rawOutput"], json!({"content": "hello from acp"}));
    assert!(claude.to_string().len() < 500);
    assert!(acp.to_string().len() < 500);
}

#[test]
fn keeps_bounded_claude_command_input_and_result_summaries() {
    let claude = project(json!({
        "itemType": "command_execution",
        "toolCallId": "claude-call-1",
        "data": {
            "toolName": "Bash",
            "input": {"command": "vp test run"},
            "result": {"type": "tool_result", "content": [
                {"type": "text", "text": "tests passed"},
                {"type": "text", "text": "x".repeat(5000)},
            ]},
        },
    }));
    let open_code = project(json!({
        "itemType": "command_execution",
        "toolCallId": "opencode-call-1",
        "data": {"tool": "bash", "state": {"status": "running", "input": {"command": "vp lint"}, "output": "x".repeat(5000)}},
    }));
    assert_eq!(claude["toolCallId"], "claude-call-1");
    assert_eq!(claude["data"]["toolName"], "Bash");
    assert_eq!(claude["data"]["command"], "vp test run");
    assert_eq!(claude["data"]["rawOutput"], json!({"content": "tests passed"}));
    assert_eq!(open_code["data"]["command"], "vp lint");
    assert!(claude.to_string().len() < 250);
    assert!(open_code.to_string().len() < 200);
}

#[test]
fn keeps_full_read_image_paths_through_repeated_projection() {
    let image_path = format!("/workspace/{}reference image.webp", "nested folder/".repeat(16));
    let projected = project_activity_payload(&activity(json!({
        "itemType": "dynamic_tool_call",
        "detail": "Read: {\"file_path\":\"truncated...\"}",
        "data": {"toolName": "Read", "input": {"file_path": image_path}, "result": {"content": "Image Size: 1280x720."}},
    })));
    let again = project_activity_payload(&projected);
    assert_eq!(projected.payload["data"]["imagePath"], json!(image_path));
    assert_eq!(again.payload["data"]["imagePath"], json!(image_path));
    let text_read = project(json!({
        "itemType": "dynamic_tool_call",
        "data": {"toolName": "Read", "input": {"file_path": "/workspace/src/index.ts"}},
    }));
    assert!(text_read["data"].get("imagePath").is_none());
}

#[test]
fn slims_codex_mcp_tool_calls() {
    let payload = project(json!({
        "itemType": "mcp_tool_call",
        "data": {"item": {
            "type": "mcpToolCall", "id": "item-1", "tool": "fetch_pr", "server": "github",
            "status": "completed", "arguments": {"pr": 42}, "durationMs": 1200,
            "result": {
                "content": [{"type": "text", "text": format!("PR body line one\n{}", "x".repeat(5000))}],
                "structuredContent": {"huge": "y".repeat(5000)},
            },
            "_meta": {"internal": true},
        }},
    }));
    let item = &payload["data"]["item"];
    assert_eq!(item["tool"], "fetch_pr");
    assert_eq!(item["server"], "github");
    assert_eq!(item["arguments"], json!({"pr": 42}));
    assert!(item.get("_meta").is_none());
    assert_eq!(item["result"], json!({"content": "PR body line one"}));
    assert!(payload.to_string().len() < 500);
}

#[test]
fn slims_claude_mcp_tool_calls() {
    let payload = project(json!({
        "itemType": "mcp_tool_call",
        "data": {
            "toolName": "mcp__github__fetch_pr",
            "input": {"pr": 42},
            "result": {"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                {"type": "text", "text": format!("first line of output\n{}", "z".repeat(5000))},
            ]},
        },
    }));
    assert_eq!(payload["data"]["toolName"], "mcp__github__fetch_pr");
    assert_eq!(payload["data"]["input"], json!({"pr": 42}));
    assert_eq!(payload["data"]["result"], json!({"content": "first line of output"}));
    assert!(payload.to_string().len() < 500);
}

#[test]
fn preserves_the_preview_page_favicon_through_result_slimming() {
    let snapshot = |truncated: bool| {
        let text = json!({
            "content": [{"type": "text", "text": "{\"url\":\"https://example.com/\"}"}],
            "structuredContent": {"url": "https://example.com/", "visibleText": "page"},
        })
        .to_string();
        let end = if truncated { text.len() - 5 } else { text.len() };
        json!({"toolName": "mcp__t3_code__preview_snapshot", "result": {"content": text[..end].to_string()}})
    };
    let page_icon = "{\"toolIcon\":{\"_tag\":\"website\",\"pageUrl\":\"https://example.com/\"}}";
    let mut cases = vec![
        json!({"item": {"server": "t3-code", "tool": "preview_open", "result": {"structuredContent": {"url": "https://example.com/"}}}}),
        json!({"toolName": "mcp__t3-code__preview_navigate", "result": {"content": "{\"url\":\"https://example.com/\"}"}}),
        json!({"tool": "t3-code_preview_status", "state": {"output": "{\"url\":\"https://example.com/\"}"}}),
        json!({"toolName": "mcp__t3_code__preview_snapshot", "result": {"content": [
            {"type": "text", "text": "{\"url\":\"https://example.com/\"}"},
            {"type": "text", "text": "Snapshot text was bounded. Omitted: accessibilityTree."},
        ]}}),
        json!({"toolName": "mcp__t3-code__preview_click", "result": {"content": page_icon}}),
        json!({"toolName": "mcp__t3_code__preview_snapshot", "result": {"content": "{\"url\":\"https://example.com/\"}\n{\"accessibilityTree\":\"truncated"}}),
        snapshot(false),
        snapshot(true),
    ];
    for action in [
        "type",
        "press",
        "scroll",
        "resize",
        "set_appearance",
        "evaluate",
        "wait_for",
        "recording_start",
        "recording_stop",
    ] {
        cases.push(json!({
            "toolName": format!("mcp__t3_code__preview_{action}"),
            "result": {"content": page_icon},
        }));
    }
    let icon = json!({"_tag": "website", "pageUrl": "https://example.com/"});
    for data in cases {
        let projected = project_activity_payload(&activity(json!({"itemType": "mcp_tool_call", "data": data.clone()})));
        assert_eq!(projected.payload["toolIcon"], icon, "{data}");
        assert_eq!(project_activity_payload(&projected).payload["toolIcon"], icon, "{data}");
    }
}

#[test]
fn keeps_the_fallback_for_unrelated_tools_failures_and_missing_urls() {
    for data in [
        json!({"toolName": "mcp__other__preview_open", "result": {"content": "{\"url\":\"https://example.com/\"}"}}),
        json!({"toolName": "mcp__t3-code__preview_evaluate", "result": {"content": "{\"url\":\"https://example.com/\"}"}}),
        json!({"toolName": "mcp__t3-code__preview_open", "result": {"isError": true, "content": "{\"url\":\"https://example.com/\"}"}}),
        json!({"toolName": "mcp__t3-code__preview_open", "result": {"content": "malformed JSON"}}),
        json!({"toolName": "mcp__t3-code__preview_open", "result": {"content": "{\"url\":\"about:blank\"}"}}),
    ] {
        let payload = project(json!({"itemType": "mcp_tool_call", "data": data.clone()}));
        assert!(payload.get("toolIcon").is_none(), "{data}");
    }
}

#[test]
fn passes_task_lifecycle_payloads_through_untouched() {
    let source = json!({
        "taskId": "task-9", "title": "Audit auth", "role": "explorer", "model": "opus",
        "effort": "high", "workflowName": "audit-flow", "phases": [{"index": 0, "title": "Audit"}],
        "typedUsage": {"totalTokens": 1200}, "runHandles": {"runId": "run-1", "scriptPath": "/tmp/wf.js"},
        "timelineBypass": true,
    });
    assert_eq!(project(source.clone()), source);
}

#[test]
fn summarizes_long_lines_on_utf16_boundaries() {
    assert_eq!(summarize_tool_text_output("short"), Some("short".into()));
    let long = format!("{}  tail", "é".repeat(90));
    let summary = summarize_tool_text_output(&long).unwrap();
    assert!(summary.ends_with('…'));
    assert_eq!(crate::js::utf16_len(&summary), 84);
    assert_eq!(summarize_tool_text_output("\n\n"), None);
    assert_eq!(summarize_tool_text_output(&"```\n".repeat(1500)), Some("1,500 lines".into()));
}
