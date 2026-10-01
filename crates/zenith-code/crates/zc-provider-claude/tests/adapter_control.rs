//! `ClaudeAdapter.test.ts`, part 5: approvals, user input, plan mode, model switches, resume
//! cursors, rewind, native logging.

mod support;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use support::*;
use tokio_util::sync::CancellationToken;
use zc_contracts::{ApprovalRequestId, ProviderApprovalDecision};
use zc_ports::adapter::ProviderAdapter;
use zc_provider_claude::history::{HistoryError, HistoryOps};
use zc_provider_claude::mapping::NativeEventSink;
use zc_provider_claude::query::{CanUseToolRequest, PermissionResult, UserDialogRequest};

const ORIGINAL_SESSION: &str = "550e8400-e29b-41d4-a716-446655440010";
const FORK_SESSION: &str = "550e8400-e29b-41d4-a716-446655440020";

fn can_use_tool(
    harness: &Harness,
    tool: &str,
    input: Value,
    suggestions: Option<Vec<Value>>,
    tool_use_id: &str,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<PermissionResult> {
    let callbacks = harness.factory.last().callbacks.clone();
    let request = CanUseToolRequest {
        tool_name: tool.into(),
        input,
        suggestions,
        tool_use_id: Some(tool_use_id.into()),
        agent_id: None,
        request_id: format!("req-{tool_use_id}"),
    };
    tokio::spawn(async move { callbacks.can_use_tool(request, cancel).await })
}

fn request_id(event: &Value) -> ApprovalRequestId {
    ApprovalRequestId::new(event["requestId"].as_str().unwrap())
}

#[tokio::test]
async fn bridges_approval_request_response_lifecycle_through_can_use_tool() {
    let mut harness = Harness::default();
    harness.start(json!({"runtimeMode": "approval-required"})).await;
    harness.take(3).await;
    harness.send(json!({"input": "approve this"})).await;
    harness.take(1).await;
    harness.emit(stream_event(
        "sdk-session-approval-1",
        "stream-approval-thread",
        json!({"type": "message_start", "message": {"id": "msg-approval-thread"}}),
    ));
    assert_eq!(harness.next_event().await["type"], json!("thread.started"));
    let permission = can_use_tool(
        &harness,
        "Bash",
        json!({"command": "pwd"}),
        Some(vec![json!({"type": "setMode", "mode": "default", "destination": "session"})]),
        "tool-use-1",
        CancellationToken::new(),
    );
    let requested = harness.next_event().await;
    assert_eq!(requested["type"], json!("request.opened"));
    assert_eq!(requested["providerRefs"], json!({"providerItemId": "tool-use-1"}));
    assert_eq!(requested["payload"]["requestType"], json!("command_execution_approval"));
    assert_eq!(requested["payload"]["detail"], json!("Bash: pwd"));
    assert_eq!(
        requested["payload"]["args"],
        json!({"toolName": "Bash", "input": {"command": "pwd"}, "toolUseId": "tool-use-1"})
    );
    harness
        .adapter
        .respond_to_request(&THREAD_ID.into(), &request_id(&requested), ProviderApprovalDecision::Accept)
        .await
        .unwrap();
    let resolved = harness.next_event().await;
    assert_eq!(resolved["type"], json!("request.resolved"));
    assert_eq!(resolved["requestId"], requested["requestId"]);
    assert_eq!(resolved["payload"]["decision"], json!("accept"));
    assert_eq!(resolved["providerRefs"], json!({"providerItemId": "tool-use-1"}));
    let result = permission.await.unwrap();
    assert!(matches!(result, PermissionResult::Allow { .. }));
    assert_eq!(
        result.to_response(Some("tool-use-1")),
        json!({"behavior": "allow", "updatedInput": {"command": "pwd"}, "toolUseID": "tool-use-1"})
    );
}

#[tokio::test]
async fn accept_for_session_returns_session_scoped_permission_updates() {
    let mut harness = Harness::default();
    harness.start(json!({"runtimeMode": "approval-required"})).await;
    harness.take(3).await;
    harness.send(json!({"input": "approve this for the session"})).await;
    harness.take(1).await;
    let mcp = can_use_tool(
        &harness,
        "mcp__linear__create_issue",
        json!({"title": "hello"}),
        Some(vec![]),
        "tool-use-mcp-1",
        CancellationToken::new(),
    );
    let requested = harness.next_event().await;
    harness
        .adapter
        .respond_to_request(&THREAD_ID.into(), &request_id(&requested), ProviderApprovalDecision::AcceptForSession)
        .await
        .unwrap();
    harness.next_event().await;
    match mcp.await.unwrap() {
        PermissionResult::Allow { updated_permissions, .. } => assert_eq!(
            updated_permissions,
            Some(vec![
                json!({"type": "addRules", "rules": [{"toolName": "mcp__linear__create_issue"}], "behavior": "allow", "destination": "session"})
            ])
        ),
        other => panic!("{other:?}"),
    }
    let bash = can_use_tool(
        &harness,
        "Bash",
        json!({"command": "git status"}),
        Some(vec![
            json!({"type": "addRules", "rules": [{"toolName": "Bash", "ruleContent": "git status"}], "behavior": "allow", "destination": "localSettings"}),
        ]),
        "tool-use-bash-1",
        CancellationToken::new(),
    );
    let requested = harness.next_event().await;
    harness
        .adapter
        .respond_to_request(&THREAD_ID.into(), &request_id(&requested), ProviderApprovalDecision::AcceptForSession)
        .await
        .unwrap();
    harness.next_event().await;
    match bash.await.unwrap() {
        PermissionResult::Allow { updated_permissions, .. } => assert_eq!(
            updated_permissions,
            Some(vec![
                json!({"type": "addRules", "rules": [{"toolName": "Bash", "ruleContent": "git status"}], "behavior": "allow", "destination": "session"})
            ])
        ),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn classifies_agent_tools_and_read_only_claude_tools_correctly_for_approvals() {
    let mut harness = Harness::default();
    harness.start(json!({"runtimeMode": "approval-required"})).await;
    harness.take(3).await;
    for (tool, input, expected) in [
        ("Agent", json!({}), "dynamic_tool_call"),
        ("Grep", json!({"pattern": "foo", "path": "src"}), "file_read_approval"),
    ] {
        let pending = can_use_tool(&harness, tool, input, None, &format!("tool-{tool}"), CancellationToken::new());
        let requested = harness.next_event().await;
        assert_eq!(requested["payload"]["requestType"], json!(expected));
        harness
            .adapter
            .respond_to_request(&THREAD_ID.into(), &request_id(&requested), ProviderApprovalDecision::Accept)
            .await
            .unwrap();
        harness.next_event().await;
        pending.await.unwrap();
    }
}

#[tokio::test]
async fn declines_and_cancels_approvals_with_the_sdk_messages() {
    let mut harness = Harness::default();
    harness.start(json!({"runtimeMode": "approval-required"})).await;
    harness.take(3).await;
    let declined = can_use_tool(&harness, "Bash", json!({"command": "rm -rf x"}), None, "tool-decline", CancellationToken::new());
    let requested = harness.next_event().await;
    harness
        .adapter
        .respond_to_request(&THREAD_ID.into(), &request_id(&requested), ProviderApprovalDecision::Decline)
        .await
        .unwrap();
    harness.next_event().await;
    assert_eq!(
        declined.await.unwrap(),
        PermissionResult::Deny {
            message: "User declined tool execution.".into()
        }
    );
    let cancel = CancellationToken::new();
    let cancelled = can_use_tool(&harness, "Bash", json!({"command": "ls"}), None, "tool-cancel", cancel.clone());
    harness.next_event().await;
    cancel.cancel();
    let resolved = harness.next_event().await;
    assert_eq!(resolved["payload"]["decision"], json!("cancel"));
    assert_eq!(
        cancelled.await.unwrap(),
        PermissionResult::Deny {
            message: "User cancelled tool execution.".into()
        }
    );
    let error = harness
        .adapter
        .respond_to_request(&THREAD_ID.into(), &ApprovalRequestId::new("unknown"), ProviderApprovalDecision::Accept)
        .await
        .unwrap_err();
    assert_eq!(err_json(&error)["detail"], json!("Unknown pending approval request: unknown"));
}

#[tokio::test]
async fn stop_settles_pending_approvals_before_the_session_exits() {
    let mut harness = Harness::default();
    harness.start(json!({"runtimeMode": "approval-required"})).await;
    harness.take(3).await;
    let pending = can_use_tool(&harness, "Bash", json!({"command": "ls"}), None, "tool-stop", CancellationToken::new());
    harness.next_event().await;
    harness.adapter.stop_session(&THREAD_ID.into()).await.unwrap();
    let events = harness.take_until("session.exited").await;
    assert_eq!(types(&events), vec!["request.resolved", "request.resolved", "session.exited"]);
    assert_eq!(events[0]["providerRefs"], json!({}));
    assert_eq!(events[1]["providerRefs"], json!({"providerItemId": "tool-stop"}));
    assert_eq!(
        pending.await.unwrap(),
        PermissionResult::Deny {
            message: "User cancelled tool execution.".into()
        }
    );
}

#[tokio::test]
async fn passes_claude_resume_ids_without_pinning_a_stale_assistant_checkpoint() {
    let harness = Harness::default();
    let session = harness
        .start(json!({"threadId": "thread-claude-resume", "resumeCursor": {"threadId": "resume-thread-1", "resume": "550e8400-e29b-41d4-a716-446655440000", "resumeSessionAt": "assistant-99", "turnCount": 3}}))
        .await;
    assert_eq!(session.thread_id.as_str(), "thread-claude-resume");
    assert_eq!(
        session.resume_cursor,
        Some(json!({"threadId": "thread-claude-resume", "resume": "550e8400-e29b-41d4-a716-446655440000", "resumeSessionAt": "assistant-99", "turnCount": 3}))
    );
    let options = harness.factory.last().options.clone();
    assert_eq!(options.resume.as_deref(), Some("550e8400-e29b-41d4-a716-446655440000"));
    assert_eq!(options.session_id, None);
    assert_eq!(options.resume_session_at, None);
}

#[tokio::test]
async fn preserves_durable_resume_ids_across_claude_resume_hooks() {
    let mut harness = Harness::default();
    let durable = "550e8400-e29b-41d4-a716-446655440000";
    let transient = "7368d0c7-40a3-4d8a-bcc1-ac80c49f2719";
    harness.start(json!({"threadId": "thread-claude-resume", "resumeCursor": {"threadId": "thread-claude-resume", "resume": durable, "resumeSessionAt": "assistant-99", "turnCount": 3}})).await;
    harness.emit(json!({"type": "system", "subtype": "hook_started", "hook_id": "resume-hook-1", "hook_name": "SessionStart:resume", "hook_event": "SessionStart", "session_id": transient, "uuid": "h1"}));
    harness.emit(json!({"type": "system", "subtype": "hook_response", "hook_id": "resume-hook-1", "hook_name": "SessionStart:resume", "hook_event": "SessionStart", "output": "", "stdout": "", "stderr": "", "outcome": "success", "session_id": transient, "uuid": "h2"}));
    harness.emit(json!({"type": "system", "subtype": "init", "apiKeySource": "none", "claude_code_version": "test", "cwd": "/tmp/claude-adapter-test", "tools": [], "mcp_servers": [], "model": STANDARD,
        "permissionMode": "bypassPermissions", "slash_commands": [], "output_style": "default", "skills": [], "plugins": [], "session_id": durable, "uuid": "resume-init"}));
    let events = harness.take(7).await;
    let started = of_type(&events, "thread.started");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["payload"], json!({"providerThreadId": durable}));
    assert_eq!(
        first_of(&events, "hook.started")["payload"],
        json!({"hookId": "resume-hook-1", "hookName": "SessionStart:resume", "hookEvent": "SessionStart"})
    );
    assert_eq!(
        first_of(&events, "hook.completed")["payload"],
        json!({"hookId": "resume-hook-1", "outcome": "success", "output": "", "stdout": "", "stderr": ""})
    );
    let sessions = harness.adapter.list_sessions().await;
    assert_eq!(sessions[0].resume_cursor.as_ref().unwrap()["resume"], json!(durable));
}

#[tokio::test]
async fn uses_an_app_generated_claude_session_id_for_fresh_sessions() {
    let harness = Harness::default();
    let session = harness.start(json!({})).await;
    let cursor = session.resume_cursor.unwrap();
    assert_eq!(cursor["threadId"], json!(THREAD_ID));
    assert_eq!(cursor["turnCount"], json!(0));
    let resume = cursor["resume"].as_str().unwrap();
    assert!(zc_provider_claude::mapping::is_uuid(resume));
    let options = harness.factory.last().options.clone();
    assert_eq!(options.resume, None);
    assert_eq!(options.session_id.as_deref(), Some(resume));
}

/// `getSessionMessages` / `forkSession` doubles.
struct ScriptedHistory {
    messages: Box<MessagesFor>,
    /// `(session id, dir, up to message id)`.
    fork_calls: Mutex<Vec<ForkCall>>,
}

type ForkCall = (String, Option<String>, String);
type MessagesFor = dyn Fn(&str) -> Vec<Value> + Send + Sync;

impl HistoryOps for ScriptedHistory {
    fn get_session_messages(&self, session_id: &str, _dir: Option<&str>) -> Result<Vec<Value>, HistoryError> {
        Ok((self.messages)(session_id))
    }

    fn fork_session(&self, session_id: &str, dir: Option<&str>, up_to_message_id: &str) -> Result<String, HistoryError> {
        self.fork_calls
            .lock()
            .unwrap()
            .push((session_id.into(), dir.map(str::to_string), up_to_message_id.into()));
        Ok(FORK_SESSION.into())
    }
}

fn history_message(kind: &str, uuid: &str, session_id: &str, content: Option<Value>) -> Value {
    let content = content.unwrap_or_else(|| match kind {
        "user" => json!("prompt"),
        "assistant" => json!([]),
        _ => json!({"subtype": "init"}),
    });
    let message = if kind == "system" { content } else { json!({"content": content}) };
    json!({"type": kind, "uuid": uuid, "session_id": session_id, "parent_tool_use_id": null, "parent_agent_id": null, "message": message})
}

type TurnIds = Arc<Mutex<Vec<String>>>;

fn scripted(build: impl Fn(&str, &[String]) -> Vec<Value> + Send + Sync + 'static) -> (Arc<ScriptedHistory>, TurnIds) {
    let turns: TurnIds = Arc::default();
    let captured = turns.clone();
    let history = Arc::new(ScriptedHistory {
        messages: Box::new(move |session| build(session, &captured.lock().unwrap())),
        fork_calls: Mutex::default(),
    });
    (history, turns)
}

async fn send_completed_turn(harness: &mut Harness, input: &str) -> String {
    let turn = harness.send(json!({"input": input})).await;
    harness.emit(json!({"type": "result", "subtype": "success", "is_error": false, "errors": [], "session_id": ORIGINAL_SESSION, "uuid": format!("result-{}", turn.turn_id)}));
    harness.take_until("turn.completed").await;
    turn.turn_id.as_str().to_string()
}

fn with_history(history: Arc<ScriptedHistory>) -> Harness {
    Harness::new(HarnessConfig {
        history: Some(history),
        ..HarnessConfig::default()
    })
}

async fn cursor(harness: &Harness) -> Value {
    harness.adapter.list_sessions().await[0].resume_cursor.clone().unwrap()
}

#[tokio::test]
async fn rewinds_a_steered_claude_turn_after_recovery_and_preserves_fork_boundaries() {
    let legacy = Arc::new(Mutex::new(false));
    let missing = Arc::new(Mutex::new(false));
    let (legacy_flag, missing_flag) = (legacy.clone(), missing.clone());
    let (history, turns) = scripted(move |session, turns| {
        let (first, second) = (turns.first().cloned().unwrap_or_default(), turns.get(1).cloned().unwrap_or_default());
        let all = vec![
            history_message("user", &first, ORIGINAL_SESSION, Some(json!("first"))),
            history_message("assistant", "assistant-1", ORIGINAL_SESSION, None),
            history_message("user", "tool-result-1", ORIGINAL_SESSION, Some(json!([{"type": "tool_result"}]))),
            history_message("assistant", "assistant-1-final", ORIGINAL_SESSION, None),
            history_message("user", &second, ORIGINAL_SESSION, Some(json!("second"))),
            history_message("assistant", "assistant-2", ORIGINAL_SESSION, None),
            history_message("user", "steer", session, Some(json!("steer the second turn"))),
            history_message("assistant", "assistant-steer", session, None),
        ];
        if session.ends_with("0020") {
            return all[..4]
                .iter()
                .map(|m| with_uuid(m, &format!("fork-{}", m["uuid"].as_str().unwrap())))
                .collect();
        }
        if *legacy_flag.lock().unwrap() {
            return all[..6].to_vec();
        }
        if *missing_flag.lock().unwrap() {
            return all.into_iter().filter(|m| m["uuid"] != json!(second)).collect();
        }
        all
    });
    let mut harness = with_history(history.clone());
    harness.start(json!({})).await;
    let first = send_completed_turn(&mut harness, "first").await;
    turns.lock().unwrap().push(first.clone());
    let second = harness.send(json!({"input": "second"})).await;
    turns.lock().unwrap().push(second.turn_id.as_str().to_string());
    let steer = harness.send(json!({"input": "steer the second turn"})).await;
    assert_eq!(steer.turn_id, second.turn_id);
    harness.emit(result_success(ORIGINAL_SESSION, "result-second"));
    let completed = harness.take_until("turn.completed").await;
    assert_eq!(completed.last().unwrap()["turnId"], json!(second.turn_id.as_str()));
    assert_eq!(harness.adapter.read_thread(&THREAD_ID.into()).await.unwrap().turns.len(), 2);
    let saved_cursor = cursor(&harness).await;
    harness.adapter.stop_session(&THREAD_ID.into()).await.unwrap();

    *legacy.lock().unwrap() = true;
    harness
        .start(json!({"resumeCursor": {"threadId": THREAD_ID, "resume": ORIGINAL_SESSION, "turnCount": 1}}))
        .await;
    let created_before = harness.factory.count();
    let error = harness.adapter.rollback_thread(&THREAD_ID.into(), 1).await.unwrap_err();
    assert!(error.to_string().contains("exact Claude turn boundary is unavailable"), "{error}");
    assert!(history.fork_calls.lock().unwrap().is_empty());
    assert_eq!(harness.factory.count(), created_before);
    assert_eq!(harness.adapter.list_sessions().await.len(), 1);
    harness.adapter.stop_session(&THREAD_ID.into()).await.unwrap();
    *legacy.lock().unwrap() = false;

    harness.start(json!({"resumeCursor": saved_cursor})).await;
    *missing.lock().unwrap() = true;
    let error = harness.adapter.rollback_thread(&THREAD_ID.into(), 1).await.unwrap_err();
    assert!(error.to_string().contains("exact Claude turn boundary is unavailable"));
    assert!(history.fork_calls.lock().unwrap().is_empty());
    *missing.lock().unwrap() = false;

    let recovered = harness.query();
    assert_eq!(recovered.close_calls.load(Ordering::SeqCst), 0);
    harness.adapter.rollback_thread(&THREAD_ID.into(), 1).await.unwrap();
    assert_eq!(recovered.close_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        *history.fork_calls.lock().unwrap(),
        vec![(ORIGINAL_SESSION.to_string(), None, "assistant-1-final".to_string())]
    );
    let fork_options = harness.factory.last().options.clone();
    assert_eq!(fork_options.resume.as_deref(), Some(FORK_SESSION));
    assert_eq!(fork_options.resume_session_at, None);
    assert!(!fork_options.fork_session);
    assert_eq!(
        cursor(&harness).await,
        json!({"threadId": THREAD_ID, "resume": FORK_SESSION, "turnCount": 1, "turnStartMessageIds": [format!("fork-{first}")]})
    );

    harness.adapter.rollback_thread(&THREAD_ID.into(), 2).await.unwrap();
    let reset = harness.factory.last().options.clone();
    assert_eq!(reset.resume, None);
    assert!(reset.session_id.is_some());
}

fn with_uuid(message: &Value, uuid: &str) -> Value {
    let mut next = message.clone();
    next["uuid"] = json!(uuid);
    next
}

#[tokio::test]
async fn completed_turns_keep_their_ids_but_not_the_sdk_messages() {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    let turn = harness.send(json!({"input": "hello"})).await;
    harness.emit(assistant(
        "sdk-session-1",
        "assistant-1",
        "assistant-message-1",
        json!([{"type": "text", "text": "Hi"}]),
    ));
    harness.emit(result_success("sdk-session-1", "result-1"));
    harness.take_until("turn.completed").await;
    let snapshot = harness.adapter.read_thread(&THREAD_ID.into()).await.unwrap();
    assert_eq!(snapshot.turns.len(), 1);
    assert_eq!(snapshot.turns[0].id, turn.turn_id);
    assert!(snapshot.turns[0].items.is_empty());
}

async fn two_turn_rewind(
    fork: impl Fn(&str, &str) -> Vec<Value> + Send + Sync + 'static,
    original: impl Fn(&str, &str) -> Vec<Value> + Send + Sync + 'static,
) -> (Harness, Arc<ScriptedHistory>, String, String) {
    let (history, turns) = scripted(move |session, turns| {
        let (first, second) = (turns.first().cloned().unwrap_or_default(), turns.get(1).cloned().unwrap_or_default());
        if session == FORK_SESSION {
            fork(&first, session)
        } else {
            original(&first, &second)
        }
    });
    let mut harness = with_history(history.clone());
    harness.start(json!({})).await;
    let first = send_completed_turn(&mut harness, "first").await;
    turns.lock().unwrap().push(first.clone());
    let second = send_completed_turn(&mut harness, "second").await;
    turns.lock().unwrap().push(second.clone());
    (harness, history, first, second)
}

#[tokio::test]
async fn rewinds_claude_history_when_the_fork_omits_retained_system_messages() {
    let (harness, history, first, _) = two_turn_rewind(
        |first, session| {
            vec![
                history_message("user", &format!("fork-{first}"), session, Some(json!("first"))),
                history_message("assistant", "fork-assistant-1", session, None),
            ]
        },
        |first, second| {
            vec![
                history_message("system", "system-init", ORIGINAL_SESSION, None),
                history_message("user", first, ORIGINAL_SESSION, Some(json!("first"))),
                history_message("assistant", "assistant-1", ORIGINAL_SESSION, None),
                history_message("system", "compact-boundary", ORIGINAL_SESSION, Some(json!({"subtype": "compact_boundary"}))),
                history_message("user", second, ORIGINAL_SESSION, Some(json!("second"))),
                history_message("assistant", "assistant-2", ORIGINAL_SESSION, None),
            ]
        },
    )
    .await;
    let snapshot = harness.adapter.rollback_thread(&THREAD_ID.into(), 1).await.unwrap();
    assert_eq!(snapshot.turns.len(), 1);
    assert_eq!(
        *history.fork_calls.lock().unwrap(),
        vec![(ORIGINAL_SESSION.to_string(), None, "compact-boundary".to_string())]
    );
    assert_eq!(
        cursor(&harness).await,
        json!({"threadId": THREAD_ID, "resume": FORK_SESSION, "turnCount": 1, "turnStartMessageIds": [format!("fork-{first}")]})
    );
}

#[tokio::test]
async fn rewinds_claude_history_when_the_fork_inserts_extra_system_messages_or_keeps_a_compact_prefix() {
    let original = |first: &str, second: &str| {
        vec![
            history_message("user", first, ORIGINAL_SESSION, Some(json!("first"))),
            history_message("assistant", "assistant-1", ORIGINAL_SESSION, None),
            history_message("user", second, ORIGINAL_SESSION, Some(json!("second"))),
            history_message("assistant", "assistant-2", ORIGINAL_SESSION, None),
        ]
    };
    let (harness, _, first, _) = two_turn_rewind(
        |first, session| {
            vec![
                history_message("system", "fork-system-init", session, None),
                history_message("user", &format!("fork-{first}"), session, Some(json!("first"))),
                history_message("assistant", "fork-assistant-1", session, None),
            ]
        },
        original,
    )
    .await;
    harness.adapter.rollback_thread(&THREAD_ID.into(), 1).await.unwrap();
    assert_eq!(
        cursor(&harness).await,
        json!({"threadId": THREAD_ID, "resume": FORK_SESSION, "turnCount": 1, "turnStartMessageIds": [format!("fork-{first}")]})
    );

    let (harness, _, first, _) = two_turn_rewind(
        |first, session| {
            vec![
                history_message("user", "fork-compacted-user", session, Some(json!("earlier compacted turn"))),
                history_message("assistant", "fork-compacted-assistant", session, None),
                history_message("user", &format!("fork-{first}"), session, Some(json!("first"))),
                history_message("assistant", "fork-assistant-1", session, None),
            ]
        },
        original,
    )
    .await;
    harness.adapter.rollback_thread(&THREAD_ID.into(), 1).await.unwrap();
    assert_eq!(
        cursor(&harness).await,
        json!({"threadId": THREAD_ID, "resume": FORK_SESSION, "turnCount": 1, "turnStartMessageIds": [format!("fork-{first}")]})
    );
}

#[tokio::test]
async fn rejects_a_claude_fork_that_drops_a_retained_user_turn() {
    let (harness, _, _, _) = two_turn_rewind(
        |_, session| vec![history_message("assistant", "fork-assistant-1", session, None)],
        |first, second| {
            vec![
                history_message("user", first, ORIGINAL_SESSION, Some(json!("first"))),
                history_message("assistant", "assistant-1", ORIGINAL_SESSION, None),
                history_message("user", second, ORIGINAL_SESSION, Some(json!("second"))),
                history_message("assistant", "assistant-2", ORIGINAL_SESSION, None),
            ]
        },
    )
    .await;
    let error = harness.adapter.rollback_thread(&THREAD_ID.into(), 1).await.unwrap_err();
    assert!(error.to_string().contains("did not preserve the retained turn boundaries"));
    assert_eq!(harness.query().close_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn rejects_a_claude_fork_that_restores_messages_or_changes_retained_content() {
    for scenario in ["restores", "changes"] {
        let (history, turns) = scripted(move |session, turns| {
            let history: Vec<Value> = turns
                .iter()
                .enumerate()
                .flat_map(|(index, turn)| {
                    vec![
                        history_message("user", turn, ORIGINAL_SESSION, Some(json!(format!("prompt {}", index + 1)))),
                        history_message(
                            "assistant",
                            &format!("assistant-{}", index + 1),
                            ORIGINAL_SESSION,
                            Some(json!([{"type": "text", "text": format!("reply {}", index + 1)}])),
                        ),
                    ]
                })
                .collect();
            if session != FORK_SESSION {
                return history;
            }
            let mut fork: Vec<Value> = history[..4]
                .iter()
                .map(|m| {
                    let mut next = with_uuid(m, &format!("fork-{}", m["uuid"].as_str().unwrap()));
                    next["session_id"] = json!(session);
                    next
                })
                .collect();
            if scenario == "restores" {
                fork.insert(
                    2,
                    history_message("user", "fork-restored-steer", session, Some(json!("earlier steer omitted by compaction"))),
                );
                fork.insert(
                    3,
                    history_message(
                        "assistant",
                        "fork-restored-reply",
                        session,
                        Some(json!([{"type": "text", "text": "earlier steering reply"}])),
                    ),
                );
            } else {
                fork[1] = history_message(
                    "assistant",
                    "fork-assistant-1",
                    session,
                    Some(json!([{"type": "text", "text": "different retained reply"}])),
                );
            }
            fork
        });
        let mut harness = with_history(history);
        harness.start(json!({})).await;
        for index in 0..3 {
            let turn = send_completed_turn(&mut harness, &format!("prompt {}", index + 1)).await;
            turns.lock().unwrap().push(turn);
        }
        let before = cursor(&harness).await;
        let created = harness.factory.count();
        let error = harness.adapter.rollback_thread(&THREAD_ID.into(), 1).await.unwrap_err();
        assert!(error.to_string().contains("did not preserve the retained turn boundaries"), "{scenario}");
        assert_eq!(harness.factory.count(), created);
        assert_eq!(harness.query().close_calls.load(Ordering::SeqCst), 0);
        assert_eq!(cursor(&harness).await, before);
    }
}

#[tokio::test]
async fn rewinds_two_of_three_turns_and_only_the_latest() {
    for (num_turns, fork_len, expected_ids) in [(2u32, 2usize, 1usize), (1, 5, 2)] {
        let (history, turns) = scripted(move |session, turns| {
            let ids: Vec<String> = (0..3).map(|i| turns.get(i).cloned().unwrap_or_default()).collect();
            if session == FORK_SESSION {
                let all = [
                    history_message("user", &format!("fork-{}", ids[0]), session, Some(json!("first"))),
                    history_message("assistant", "fork-assistant-1", session, None),
                    history_message("system", "fork-notice", session, None),
                    history_message("user", &format!("fork-{}", ids[1]), session, Some(json!("second"))),
                    history_message("assistant", "fork-assistant-2", session, None),
                ];
                return all[..fork_len].to_vec();
            }
            vec![
                history_message("system", "system-init", ORIGINAL_SESSION, None),
                history_message("user", &ids[0], ORIGINAL_SESSION, Some(json!("first"))),
                history_message("assistant", "assistant-1", ORIGINAL_SESSION, None),
                history_message("user", &ids[1], ORIGINAL_SESSION, Some(json!("second"))),
                history_message("assistant", "assistant-2", ORIGINAL_SESSION, None),
                history_message("user", &ids[2], ORIGINAL_SESSION, Some(json!("third"))),
                history_message("assistant", "assistant-3", ORIGINAL_SESSION, None),
            ]
        });
        let mut harness = with_history(history);
        harness.start(json!({})).await;
        for input in ["first", "second", "third"] {
            let turn = send_completed_turn(&mut harness, input).await;
            turns.lock().unwrap().push(turn);
        }
        let snapshot = harness.adapter.rollback_thread(&THREAD_ID.into(), num_turns).await.unwrap();
        assert_eq!(snapshot.turns.len(), expected_ids);
        let ids: Vec<Value> = turns.lock().unwrap()[..expected_ids].iter().map(|id| json!(format!("fork-{id}"))).collect();
        assert_eq!(
            cursor(&harness).await,
            json!({"threadId": THREAD_ID, "resume": FORK_SESSION, "turnCount": expected_ids, "turnStartMessageIds": ids})
        );
    }
}

#[tokio::test]
async fn resets_a_claude_thread_when_rewind_removes_every_recorded_turn() {
    let (history, _) = scripted(|_, _| vec![history_message("user", "unused", ORIGINAL_SESSION, Some(json!("first")))]);
    let mut harness = with_history(history.clone());
    harness.start(json!({})).await;
    send_completed_turn(&mut harness, "first").await;
    send_completed_turn(&mut harness, "second").await;
    let snapshot = harness.adapter.rollback_thread(&THREAD_ID.into(), 2).await.unwrap();
    assert!(snapshot.turns.is_empty());
    assert!(history.fork_calls.lock().unwrap().is_empty());
    let reset = harness.factory.last().options.clone();
    assert_eq!(reset.resume, None);
    assert!(reset.session_id.is_some());
}

#[tokio::test]
async fn updates_the_model_on_send_turn_only_when_the_effective_api_model_changes() {
    let harness = Harness::default();
    harness.start(json!({})).await;
    harness
        .send(json!({"input": "hello", "modelSelection": {"instanceId": "claudeAgent", "model": CAPABLE}}))
        .await;
    assert_eq!(*harness.query().set_model_calls.lock().unwrap(), vec![Some(format!("{CAPABLE}[expanded]"))]);

    let custom = Harness::new(HarnessConfig {
        instance_id: Some("claude_openrouter".into()),
        ..HarnessConfig::default()
    });
    custom.start(json!({})).await;
    custom
        .send(json!({"input": "hello", "modelSelection": {"instanceId": "claude_openrouter", "model": "openai/gpt-5.5"}}))
        .await;
    assert_eq!(*custom.query().set_model_calls.lock().unwrap(), vec![Some("openai/gpt-5.5".to_string())]);

    let same = Harness::default();
    let selection = json!({"instanceId": "claudeAgent", "model": CAPABLE});
    same.start(json!({"modelSelection": selection})).await;
    same.send(json!({"input": "hello", "modelSelection": selection})).await;
    same.send(json!({"input": "hello again", "modelSelection": selection})).await;
    assert!(same.query().set_model_calls.lock().unwrap().is_empty());

    let changes = Harness::default();
    changes.start(json!({})).await;
    changes.send(json!({"input": "hello", "modelSelection": {"instanceId": "claudeAgent", "model": CAPABLE, "options": [{"id": "contextWindow", "value": "expanded"}]}})).await;
    changes.send(json!({"input": "again", "modelSelection": {"instanceId": "claudeAgent", "model": CAPABLE, "options": [{"id": "contextWindow", "value": "standard"}]}})).await;
    assert_eq!(
        *changes.query().set_model_calls.lock().unwrap(),
        vec![Some(format!("{CAPABLE}[expanded]")), Some(CAPABLE.to_string())]
    );
}

#[tokio::test]
async fn sets_and_restores_plan_permission_mode() {
    let harness = Harness::default();
    harness.start(json!({})).await;
    harness.send(json!({"input": "hello"})).await;
    assert!(harness.query().set_permission_mode_calls.lock().unwrap().is_empty());
    for (mode, base) in [
        ("full-access", "bypassPermissions"),
        ("approval-required", "default"),
        ("auto-accept-edits", "acceptEdits"),
    ] {
        let mut harness = Harness::default();
        harness.start(json!({"runtimeMode": mode})).await;
        harness.send(json!({"input": "plan this", "interactionMode": "plan"})).await;
        harness.emit(result_success(&format!("sdk-session-{mode}"), &format!("result-{mode}")));
        harness.take_until("turn.completed").await;
        harness.send(json!({"input": "now do it", "interactionMode": "default"})).await;
        assert_eq!(
            *harness.query().set_permission_mode_calls.lock().unwrap(),
            vec!["plan".to_string(), base.to_string()]
        );
    }
}

#[tokio::test]
async fn captures_exit_plan_mode_as_a_proposed_plan_and_denies_auto_exit() {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    harness.take(3).await;
    harness.send(json!({"input": "plan this", "interactionMode": "plan"})).await;
    harness.take(1).await;
    let pending = can_use_tool(
        &harness,
        "ExitPlanMode",
        json!({"plan": "# Ship it\n\n- one\n- two", "allowedPrompts": [{"tool": "Bash", "prompt": "run tests"}]}),
        None,
        "tool-exit-1",
        CancellationToken::new(),
    );
    let proposed = harness.next_event().await;
    assert_eq!(proposed["type"], json!("turn.proposed.completed"));
    assert_eq!(proposed["payload"]["planMarkdown"], json!("# Ship it\n\n- one\n- two"));
    assert_eq!(proposed["providerRefs"], json!({"providerItemId": "tool-exit-1"}));
    match pending.await.unwrap() {
        PermissionResult::Deny { message } => assert!(message.contains("captured your proposed plan")),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn extracts_proposed_plans_from_assistant_exit_plan_mode_snapshots() {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    harness.take(3).await;
    harness.send(json!({"input": "plan this", "interactionMode": "plan"})).await;
    harness.take(1).await;
    harness.emit(json!({"type": "assistant", "session_id": "sdk-session-exit-plan", "uuid": "assistant-exit-plan", "parent_tool_use_id": null,
        "message": {"model": CAPABLE, "id": "msg-exit-plan", "type": "message", "role": "assistant",
            "content": [{"type": "tool_use", "id": "tool-exit-2", "name": "ExitPlanMode", "input": {"plan": "# Final plan\n\n- capture it"}}], "stop_reason": null, "stop_sequence": null, "usage": {}}}));
    let events = harness.take_until("turn.proposed.completed").await;
    let proposed = events.last().unwrap();
    assert_eq!(proposed["payload"]["planMarkdown"], json!("# Final plan\n\n- capture it"));
    assert_eq!(proposed["providerRefs"], json!({"providerItemId": "tool-exit-2"}));
}

#[tokio::test]
async fn routes_claude_resume_compaction_through_the_shared_user_input_ui() {
    let mut harness = Harness::default();
    harness
        .start(json!({"threadId": "thread-claude-resume", "resumeCursor": {"resume": "550e8400-e29b-41d4-a716-446655440000"}}))
        .await;
    harness.take(3).await;
    let callbacks = harness.factory.last().callbacks.clone();
    let dialog = tokio::spawn(async move {
        callbacks
            .on_user_dialog(
                UserDialogRequest {
                    dialog_kind: "resume_return".into(),
                    payload: json!({"sessionAgeMinutes": 145, "estimatedTokens": 275123}),
                    tool_use_id: None,
                    request_id: "request-dialog".into(),
                },
                CancellationToken::new(),
            )
            .await
    });
    let requested = harness.next_event().await;
    assert_eq!(requested["type"], json!("user-input.requested"));
    let question = &requested["payload"]["questions"][0];
    assert_eq!(question["header"], json!("Resume session"));
    assert!(question["question"].as_str().unwrap().contains("2h 25m"));
    assert!(question["question"].as_str().unwrap().contains("275,123 tokens"));
    assert_eq!(
        question["options"].as_array().unwrap().iter().map(|o| o["label"].clone()).collect::<Vec<_>>(),
        vec![json!("Compact and continue"), json!("Keep full history"), json!("Don't ask again")]
    );
    let answers = std::collections::BTreeMap::from([(question["id"].as_str().unwrap().to_string(), json!("Compact and continue"))]);
    harness
        .adapter
        .respond_to_user_input(&"thread-claude-resume".into(), &request_id(&requested), answers)
        .await
        .unwrap();
    assert_eq!(harness.next_event().await["type"], json!("user-input.resolved"));
    assert_eq!(dialog.await.unwrap(), Some(json!({"behavior": "completed", "result": "compact"})));
    let callbacks = harness.factory.last().callbacks.clone();
    let other = callbacks
        .on_user_dialog(
            UserDialogRequest {
                dialog_kind: "other".into(),
                payload: json!({}),
                tool_use_id: None,
                request_id: "r".into(),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(other, Some(json!({"behavior": "cancelled"})));
}

fn ask_input(question: &str) -> Value {
    json!({"questions": [{"question": question, "header": "Framework", "options": [{"label": "React", "description": "React.js"}, {"label": "Vue", "description": "Vue.js"}], "multiSelect": false}]})
}

#[tokio::test]
async fn handles_ask_user_question_via_user_input_requested_resolved_lifecycle() {
    for mode in ["approval-required", "full-access"] {
        let mut harness = Harness::default();
        harness.start(json!({"runtimeMode": mode})).await;
        harness.take(3).await;
        let input = ask_input("Which framework?");
        let pending = can_use_tool(&harness, "AskUserQuestion", input.clone(), None, "tool-ask-1", CancellationToken::new());
        let requested = harness.next_event().await;
        assert_eq!(requested["type"], json!("user-input.requested"));
        assert_eq!(
            requested["payload"]["questions"],
            json!([{"id": "Which framework?", "header": "Framework", "question": "Which framework?", "options": [{"label": "React", "description": "React.js"}, {"label": "Vue", "description": "Vue.js"}], "multiSelect": false}])
        );
        assert_eq!(requested["providerRefs"], json!({"providerItemId": "tool-ask-1"}));
        let answers = std::collections::BTreeMap::from([("Which framework?".to_string(), json!("React"))]);
        harness
            .adapter
            .respond_to_user_input(&THREAD_ID.into(), &request_id(&requested), answers)
            .await
            .unwrap();
        let resolved = harness.next_event().await;
        assert_eq!(resolved["type"], json!("user-input.resolved"));
        assert_eq!(resolved["payload"]["answers"], json!({"Which framework?": "React"}));
        assert_eq!(resolved["providerRefs"], json!({"providerItemId": "tool-ask-1"}));
        match pending.await.unwrap() {
            PermissionResult::Allow { updated_input, .. } => {
                assert_eq!(updated_input["answers"], json!({"Which framework?": "React"}));
                assert_eq!(updated_input["questions"], input["questions"]);
            }
            other => panic!("{other:?}"),
        }
    }
}

#[tokio::test]
async fn denies_ask_user_question_when_the_waiting_turn_is_aborted_before_or_after_registration() {
    for pre_aborted in [false, true] {
        let mut harness = Harness::default();
        harness.start(json!({"runtimeMode": "approval-required"})).await;
        harness.take(3).await;
        let cancel = CancellationToken::new();
        if pre_aborted {
            cancel.cancel();
        }
        let pending = can_use_tool(&harness, "AskUserQuestion", ask_input("Continue?"), None, "tool-ask-abort", cancel.clone());
        let requested = harness.next_event().await;
        assert_eq!(requested["type"], json!("user-input.requested"));
        assert_eq!(requested["threadId"], json!(THREAD_ID));
        cancel.cancel();
        let resolved = harness.next_event().await;
        assert_eq!(resolved["type"], json!("user-input.resolved"));
        assert_eq!(resolved["payload"]["answers"], json!({}));
        assert_eq!(
            pending.await.unwrap(),
            PermissionResult::Deny {
                message: "User cancelled tool execution.".into()
            }
        );
    }
}

#[tokio::test]
async fn stopping_a_session_settles_pending_user_input_waits() {
    let mut harness = Harness::default();
    harness.start(json!({"runtimeMode": "approval-required"})).await;
    harness.take(3).await;
    let pending = can_use_tool(
        &harness,
        "AskUserQuestion",
        ask_input("Continue?"),
        None,
        "tool-ask-stop",
        CancellationToken::new(),
    );
    assert_eq!(harness.next_event().await["type"], json!("user-input.requested"));
    harness.adapter.stop_session(&THREAD_ID.into()).await.unwrap();
    let resolved = harness.next_event().await;
    assert_eq!(resolved["type"], json!("user-input.resolved"));
    assert_eq!(resolved["payload"]["answers"], json!({}));
    assert_eq!(harness.next_event().await["type"], json!("session.exited"));
    assert_eq!(
        pending.await.unwrap(),
        PermissionResult::Deny {
            message: "User cancelled tool execution.".into()
        }
    );
}

#[derive(Default)]
struct MemorySink(Mutex<Vec<(Value, String)>>);

impl NativeEventSink for MemorySink {
    fn write(&self, record: Value, thread_id: &str) {
        self.0.lock().unwrap().push((record, thread_id.to_string()));
    }
}

#[tokio::test]
async fn writes_provider_native_observability_records_when_enabled() {
    let sink = Arc::new(MemorySink::default());
    let temp = tempfile::tempdir().unwrap();
    let factory = Arc::new(FakeFactory::default());
    let mut options = zc_provider_claude::ClaudeAdapterOptions::new(settings(json!({})), "claudeAgent".into(), Default::default(), temp.path().to_path_buf());
    options.catalog = Arc::new(synthetic_catalog);
    options.query_factory = factory.clone();
    options.native_sink = Some(sink.clone());
    let adapter = zc_provider_claude::ClaudeAdapter::new(options);
    let mut events = adapter.subscribe_events();
    adapter
        .start(serde_json::from_value(json!({"threadId": THREAD_ID, "provider": "claudeAgent", "runtimeMode": "full-access"})).unwrap())
        .await
        .unwrap();
    let turn = adapter
        .send(serde_json::from_value(json!({"threadId": THREAD_ID, "input": "hello", "attachments": []})).unwrap())
        .await
        .unwrap();
    let query = factory.last().query.clone();
    query.emit(stream_event(
        "sdk-session-native-log",
        "stream-native-log",
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hi"}}),
    ));
    query.emit(result_success("sdk-session-native-log", "result-native-log"));
    use futures::StreamExt;
    while let Some(event) = events.next().await {
        if serde_json::to_value(&event).unwrap()["type"] == "turn.completed" {
            break;
        }
    }
    let records = sink.0.lock().unwrap();
    assert!(!records.is_empty());
    assert!(records.iter().all(|(_, thread)| thread == THREAD_ID));
    assert!(records
        .iter()
        .any(|(r, _)| r["event"]["provider"] == "claudeAgent" && r["event"]["providerThreadId"] == "sdk-session-native-log"));
    assert!(records.iter().any(|(r, _)| r["event"]["turnId"] == json!(turn.turn_id.as_str())));
    assert!(records
        .iter()
        .any(|(r, _)| r["event"]["method"] == "claude/stream_event/content_block_delta/text_delta"));
    let delta = records.iter().find(|(r, _)| r["event"]["id"] == "stream-native-log").unwrap();
    assert_eq!(delta.0["event"]["kind"], json!("notification"));
    assert_eq!(delta.0["event"]["payload"]["uuid"], json!("stream-native-log"));
}
