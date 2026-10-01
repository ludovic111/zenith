//! Opening, reading and rewinding a Codex thread (`openCodexThread`, `readCodexThread`,
//! `rollbackCodexThread`, `isRecoverableThreadResumeError` and the runtime-mode table of
//! `CodexSessionRuntime.ts`).

use std::collections::HashSet;

use serde::Deserialize;
use serde_json::{json, Map, Value};
use zc_contracts::RuntimeMode;

use crate::client::{decode_response, CodexRequester};
use crate::errors::{CodexAppServerError, RequestError, RequestOperation};

/// The approval policy, sandbox and reviewer of a runtime mode (`runtimeModeToThreadConfig`,
/// plan §4.3). The reviewer is always explicit: omitting it on resume would keep `auto_review`
/// after a mode switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadConfig {
    pub approval_policy: &'static str,
    pub sandbox: &'static str,
    pub approvals_reviewer: &'static str,
}

pub fn runtime_mode_to_thread_config(mode: RuntimeMode) -> ThreadConfig {
    match mode {
        RuntimeMode::ApprovalRequired => ThreadConfig {
            approval_policy: "untrusted",
            sandbox: "read-only",
            approvals_reviewer: "user",
        },
        RuntimeMode::AutoAcceptEdits => ThreadConfig {
            approval_policy: "on-request",
            sandbox: "workspace-write",
            approvals_reviewer: "user",
        },
        RuntimeMode::Auto => ThreadConfig {
            approval_policy: "on-request",
            sandbox: "workspace-write",
            approvals_reviewer: "auto_review",
        },
        RuntimeMode::FullAccess => ThreadConfig {
            approval_policy: "never",
            sandbox: "danger-full-access",
            approvals_reviewer: "user",
        },
    }
}

/// `runtimeModeToTurnSandboxPolicy`.
pub fn runtime_mode_to_turn_sandbox_policy(mode: RuntimeMode) -> Value {
    match mode {
        RuntimeMode::ApprovalRequired => json!({ "type": "readOnly" }),
        RuntimeMode::AutoAcceptEdits | RuntimeMode::Auto => json!({ "type": "workspaceWrite" }),
        RuntimeMode::FullAccess => json!({ "type": "dangerFullAccess" }),
    }
}

/// `buildThreadStartParams`.
pub fn build_thread_start_params(cwd: &str, mode: RuntimeMode, model: Option<&str>, service_tier: Option<&str>) -> Map<String, Value> {
    let config = runtime_mode_to_thread_config(mode);
    let mut params = Map::new();
    params.insert("cwd".into(), json!(cwd));
    params.insert("approvalPolicy".into(), json!(config.approval_policy));
    params.insert("sandbox".into(), json!(config.sandbox));
    params.insert("approvalsReviewer".into(), json!(config.approvals_reviewer));
    if let Some(model) = model {
        params.insert("model".into(), json!(model));
    }
    if let Some(tier) = service_tier {
        params.insert("serviceTier".into(), json!(tier));
    }
    params
}

const RECOVERABLE_THREAD_RESUME_ERROR_SNIPPETS: &[&str] = &[
    "not found",
    "missing thread",
    "no such thread",
    "unknown thread",
    "does not exist",
    "no rollout found",
];

/// `isRecoverableThreadResumeError`: a resume that failed because the thread is gone.
pub fn is_recoverable_thread_resume_error(error: &CodexAppServerError) -> bool {
    let message = error.to_string().to_lowercase();
    message.contains("thread") && RECOVERABLE_THREAD_RESUME_ERROR_SNIPPETS.iter().any(|snippet| message.contains(snippet))
}

/// What opening a thread yields (`CodexThreadResumeMetadata`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ThreadOpenMetadata {
    pub cwd: String,
    pub model: String,
    pub thread: ThreadRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ThreadRef {
    pub id: String,
}

/// `openCodexThread`: `thread/resume` (only the metadata is read: older providers may still
/// send history despite `excludeTurns`), falling back to `thread/start` when the saved thread
/// is gone; otherwise `thread/start`.
pub async fn open_codex_thread(
    client: &(impl CodexRequester + ?Sized),
    thread_id: &str,
    mode: RuntimeMode,
    cwd: &str,
    requested_model: Option<&str>,
    service_tier: Option<&str>,
    resume_thread_id: Option<&str>,
) -> Result<ThreadOpenMetadata, CodexAppServerError> {
    let start_params = build_thread_start_params(cwd, mode, requested_model, service_tier);
    let Some(resume_thread_id) = resume_thread_id else {
        return start_thread(client, start_params).await;
    };
    let mut resume = Map::new();
    resume.insert("threadId".into(), json!(resume_thread_id));
    resume.extend(start_params.clone());
    resume.insert("excludeTurns".into(), json!(true));
    let resumed = client
        .request_raw("thread/resume", Some(Value::Object(resume)))
        .await
        .and_then(|response| decode_response::<ThreadOpenMetadata>("thread/resume", response));
    match resumed {
        Ok(metadata) => Ok(metadata),
        Err(error) if is_recoverable_thread_resume_error(&error) => {
            tracing::warn!(
                thread_id,
                resume_thread_id,
                runtime_mode = mode.as_str(),
                %error,
                "codex app-server thread resume fell back to fresh start"
            );
            start_thread(client, start_params).await
        }
        Err(error) => Err(error),
    }
}

async fn start_thread(client: &(impl CodexRequester + ?Sized), params: Map<String, Value>) -> Result<ThreadOpenMetadata, CodexAppServerError> {
    let response = client.request_raw("thread/start", Some(Value::Object(params))).await?;
    decode_response("thread/start", response)
}

/// One turn of a thread snapshot; items as Codex sent them.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CodexThreadTurnSnapshot {
    pub id: String,
    pub items: Vec<Value>,
}

/// `CodexThreadSnapshot`.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexThreadSnapshot {
    pub thread_id: String,
    pub turns: Vec<CodexThreadTurnSnapshot>,
}

#[derive(Deserialize)]
struct HistoryMetadata {
    thread: HistoryThread,
}

#[derive(Deserialize)]
struct HistoryThread {
    #[serde(rename = "historyMode", default)]
    history_mode: Option<HistoryMode>,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum HistoryMode {
    Legacy,
    Paginated,
}

#[derive(Deserialize)]
struct ThreadReadResponse {
    thread: ThreadWithTurns,
}

#[derive(Deserialize)]
struct ThreadWithTurns {
    id: String,
    turns: Vec<CodexThreadTurnSnapshot>,
}

#[derive(Deserialize)]
struct TurnsPage {
    data: Vec<CodexThreadTurnSnapshot>,
    #[serde(rename = "nextCursor", deserialize_with = "zc_codex_protocol::serde_helpers::nullable")]
    next_cursor: Option<String>,
}

/// `readCodexThread`: legacy threads in one `thread/read`, paginated ones through
/// `thread/turns/list` (a repeated cursor is an error, not a loop).
pub async fn read_codex_thread(client: &(impl CodexRequester + ?Sized), thread_id: &str) -> Result<CodexThreadSnapshot, CodexAppServerError> {
    let metadata = client
        .request_raw("thread/read", Some(json!({ "threadId": thread_id, "includeTurns": false })))
        .await?;
    let metadata: HistoryMetadata = decode_response("thread/read", metadata)?;
    if metadata.thread.history_mode != Some(HistoryMode::Paginated) {
        let response = client
            .request_raw("thread/read", Some(json!({ "threadId": thread_id, "includeTurns": true })))
            .await?;
        let response: ThreadReadResponse = decode_response("thread/read", response)?;
        return Ok(CodexThreadSnapshot {
            thread_id: response.thread.id,
            turns: response.thread.turns,
        });
    }
    let mut turns = Vec::new();
    let mut requested: HashSet<Option<String>> = HashSet::new();
    let mut cursor: Option<String> = None;
    loop {
        if !requested.insert(cursor.clone()) {
            let mut error = RequestError::internal_error("Thread history pagination repeated a cursor.");
            error.method = Some("thread/turns/list".to_owned());
            error.operation = Some(RequestOperation::DecodePayload);
            return Err(CodexAppServerError::Request(error));
        }
        let response = client
            .request_raw(
                "thread/turns/list",
                Some(json!({ "threadId": thread_id, "cursor": cursor, "limit": 100, "sortDirection": "asc", "itemsView": "full" })),
            )
            .await?;
        let page: TurnsPage = decode_response("thread/turns/list", response)?;
        turns.extend(page.data);
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    Ok(CodexThreadSnapshot {
        thread_id: thread_id.to_owned(),
        turns,
    })
}

/// `rollbackCodexThread`: Codex replaces history at a turn boundary (`thread/revert`); it
/// rejects legacy threads, which have no rollback API since Codex 0.156.
pub async fn rollback_codex_thread(
    client: &(impl CodexRequester + ?Sized),
    thread_id: &str,
    num_turns: usize,
) -> Result<CodexThreadSnapshot, CodexAppServerError> {
    let snapshot = read_codex_thread(client, thread_id).await?;
    let retained = snapshot.turns.len().saturating_sub(num_turns);
    if let Some(first_removed) = snapshot.turns.get(retained) {
        client
            .request_raw("thread/revert", Some(json!({ "threadId": thread_id, "beforeTurnId": first_removed.id })))
            .await?;
    }
    Ok(CodexThreadSnapshot {
        thread_id: thread_id.to_owned(),
        turns: snapshot.turns.into_iter().take(retained).collect(),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;

    type Handler = Box<dyn Fn(&str, &Value) -> Result<Value, CodexAppServerError> + Send + Sync>;

    struct FakeClient {
        handler: Handler,
        calls: Mutex<Vec<(String, Value)>>,
    }

    impl FakeClient {
        fn new(handler: impl Fn(&str, &Value) -> Result<Value, CodexAppServerError> + Send + Sync + 'static) -> Self {
            Self {
                handler: Box::new(handler),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl CodexRequester for FakeClient {
        async fn request_raw(&self, method: &str, params: Option<Value>) -> Result<Value, CodexAppServerError> {
            let params = params.unwrap_or(Value::Null);
            self.calls.lock().unwrap().push((method.to_owned(), params.clone()));
            (self.handler)(method, &params)
        }
    }

    fn request_error(message: &str) -> CodexAppServerError {
        CodexAppServerError::Request(RequestError::internal_error(message))
    }

    #[tokio::test]
    async fn reverts_paginated_turns_at_the_durable_boundary() {
        for num_turns in [1usize, 2, 3, 5] {
            let retained = std::sync::Arc::new(Mutex::new(vec!["turn-1", "turn-2", "turn-3"]));
            let state = retained.clone();
            let client = FakeClient::new(move |method, params| {
                let mut retained = state.lock().unwrap();
                match method {
                    "thread/read" => {
                        assert_eq!(params["includeTurns"], false, "the legacy history API must not be used for paginated threads");
                        Ok(json!({"thread": {"historyMode": "paginated"}}))
                    }
                    "thread/turns/list" => {
                        let start = params["cursor"].as_str().map_or(0, |cursor| cursor.parse::<usize>().unwrap());
                        let ids: Vec<_> = retained.iter().skip(start).take(2).collect();
                        Ok(json!({
                            "data": ids.iter().map(|id| json!({"id": id, "items": [], "status": "completed"})).collect::<Vec<_>>(),
                            "nextCursor": if start + 2 < retained.len() { json!((start + 2).to_string()) } else { Value::Null },
                        }))
                    }
                    "thread/revert" => {
                        let before = params["beforeTurnId"].as_str().unwrap();
                        let index = retained.iter().position(|id| *id == before).unwrap();
                        retained.truncate(index);
                        Ok(json!({"thread": {"id": "thread-1", "turns": []}}))
                    }
                    other => panic!("unexpected {other}"),
                }
            });
            let expected: Vec<String> = ["turn-1", "turn-2", "turn-3"]
                .iter()
                .take(3usize.saturating_sub(num_turns))
                .map(|id| (*id).to_owned())
                .collect();
            let result = rollback_codex_thread(&client, "thread-1", num_turns).await.unwrap();
            assert_eq!(result.turns.iter().map(|turn| turn.id.clone()).collect::<Vec<_>>(), expected);
            let read = read_codex_thread(&client, "thread-1").await.unwrap();
            assert_eq!(read.turns.iter().map(|turn| turn.id.clone()).collect::<Vec<_>>(), expected);
        }
    }

    #[tokio::test]
    async fn rejects_a_pagination_cursor_cycle() {
        for cursors in [vec!["next", "next"], vec!["first", "second", "first"]] {
            let count = std::sync::Arc::new(Mutex::new(0usize));
            let state = count.clone();
            let pages = cursors.clone();
            let client = FakeClient::new(move |method, _| {
                if method == "thread/read" {
                    return Ok(json!({"thread": {"historyMode": "paginated"}}));
                }
                let mut count = state.lock().unwrap();
                assert!(*count < pages.len(), "a repeated cursor was requested");
                let next = pages[*count];
                *count += 1;
                Ok(json!({"data": [], "nextCursor": next}))
            });
            let error = read_codex_thread(&client, "thread-1").await.unwrap_err();
            assert!(matches!(error, CodexAppServerError::Request(_)));
            assert_eq!(*count.lock().unwrap(), cursors.len());
        }
    }

    #[tokio::test]
    async fn surfaces_codex_rejecting_a_legacy_revert() {
        let rejection = CodexAppServerError::Request(RequestError::invalid_request("thread/revert only supports paginated threads"));
        let returned = rejection.clone();
        let client = FakeClient::new(move |method, params| match method {
            "thread/read" if params["includeTurns"] == false => Ok(json!({"thread": {}})),
            "thread/read" => Ok(json!({"thread": {"id": "legacy-thread", "turns": [{"id": "turn-1", "items": []}]}})),
            "thread/revert" => Err(returned.clone()),
            other => panic!("unexpected {other}"),
        });
        assert_eq!(rollback_codex_thread(&client, "legacy-thread", 1).await.unwrap_err(), rejection);
    }

    fn thread_open_response(thread_id: &str) -> Value {
        json!({
            "cwd": "/tmp/project", "model": "gpt-5.3-codex", "modelProvider": "openai", "approvalPolicy": "never",
            "approvalsReviewer": "user", "sandbox": {"type": "danger-full-access"},
            "thread": {"id": thread_id, "createdAt": "2026-04-18T00:00:00.000Z", "source": {"session": "cli"}, "turns": [], "status": {"state": "idle", "activeFlags": []}}
        })
    }

    #[tokio::test]
    async fn resumes_metadata_despite_unknown_historical_values() {
        let client = FakeClient::new(|method, _| {
            assert_eq!(method, "thread/resume", "a valid resumed thread must not start fresh");
            let mut response = thread_open_response("saved-thread");
            response["thread"]["turns"] = json!([{"id": "old-turn", "status": "failed", "items": [], "error": {"message": "Historical provider error", "codexErrorInfo": "misalignment_policy_violation"}}]);
            Ok(response)
        });
        let opened = open_codex_thread(
            &client,
            "thread-1",
            RuntimeMode::Auto,
            "/tmp/project",
            Some("gpt-5.3-codex"),
            Some("fast"),
            Some("saved-thread"),
        )
        .await
        .unwrap();
        assert_eq!(
            opened,
            ThreadOpenMetadata {
                cwd: "/tmp/project".into(),
                model: "gpt-5.3-codex".into(),
                thread: ThreadRef { id: "saved-thread".into() }
            }
        );
        assert_eq!(
            client.calls.lock().unwrap().clone(),
            vec![(
                "thread/resume".to_owned(),
                json!({"threadId": "saved-thread", "cwd": "/tmp/project", "model": "gpt-5.3-codex", "serviceTier": "fast", "approvalPolicy": "on-request", "sandbox": "workspace-write", "approvalsReviewer": "auto_review", "excludeTurns": true})
            )]
        );
    }

    #[tokio::test]
    async fn rejects_malformed_resume_metadata_without_starting_fresh() {
        for invalid in [
            json!({"cwd": null}),
            json!({"model": 42}),
            json!({"thread": {"id": null}}),
            json!({"thread": {}}),
        ] {
            let client = FakeClient::new(move |method, _| {
                assert_eq!(method, "thread/resume", "invalid resume metadata must not start a fresh thread");
                let mut response = thread_open_response("saved-thread");
                for (key, value) in invalid.as_object().unwrap() {
                    response[key] = value.clone();
                }
                Ok(response)
            });
            let error = open_codex_thread(
                &client,
                "thread-1",
                RuntimeMode::FullAccess,
                "/tmp/project",
                Some("gpt-5.3-codex"),
                None,
                Some("saved-thread"),
            )
            .await
            .unwrap_err();
            let CodexAppServerError::Request(error) = error else {
                panic!("request error expected")
            };
            assert_eq!(error.operation, Some(RequestOperation::DecodePayload));
            assert_eq!(error.method.as_deref(), Some("thread/resume"));
        }
    }

    #[tokio::test]
    async fn falls_back_to_thread_start_when_resume_fails_recoverably() {
        let client = FakeClient::new(|method, _| match method {
            "thread/resume" => Err(request_error("thread not found")),
            "thread/start" => Ok(thread_open_response("fresh-thread")),
            other => panic!("unexpected {other}"),
        });
        let opened = open_codex_thread(
            &client,
            "thread-1",
            RuntimeMode::FullAccess,
            "/tmp/project",
            Some("gpt-5.3-codex"),
            None,
            Some("stale-thread"),
        )
        .await
        .unwrap();
        assert_eq!(opened.thread.id, "fresh-thread");
        assert_eq!(
            client.calls.lock().unwrap().iter().map(|(method, _)| method.clone()).collect::<Vec<_>>(),
            vec!["thread/resume", "thread/start"]
        );
    }

    #[tokio::test]
    async fn propagates_non_recoverable_resume_failures() {
        let client = FakeClient::new(|method, _| {
            assert_eq!(method, "thread/resume", "non-recoverable resume failures must not start a fresh thread");
            Err(request_error("timed out waiting for server"))
        });
        let error = open_codex_thread(
            &client,
            "thread-1",
            RuntimeMode::FullAccess,
            "/tmp/project",
            Some("gpt-5.3-codex"),
            None,
            Some("stale-thread"),
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "timed out waiting for server");
    }

    #[test]
    fn recoverable_resume_errors() {
        assert!(is_recoverable_thread_resume_error(&request_error("Thread does not exist")));
        assert!(is_recoverable_thread_resume_error(&request_error(
            "no rollout found for thread id 019fdf74-aaa9-7950-b252-7cc7a8650470"
        )));
        assert!(!is_recoverable_thread_resume_error(&request_error("Permission denied")));
        assert!(!is_recoverable_thread_resume_error(&request_error("Config file not found")));
        assert!(!is_recoverable_thread_resume_error(&request_error("Model does not exist")));
    }

    #[test]
    fn runtime_mode_table() {
        let table: Vec<_> = RuntimeMode::ALL
            .iter()
            .map(|mode| {
                let config = runtime_mode_to_thread_config(*mode);
                (mode.as_str(), config.approval_policy, config.sandbox, config.approvals_reviewer)
            })
            .collect();
        assert_eq!(
            table,
            vec![
                ("approval-required", "untrusted", "read-only", "user"),
                ("auto-accept-edits", "on-request", "workspace-write", "user"),
                ("auto", "on-request", "workspace-write", "auto_review"),
                ("full-access", "never", "danger-full-access", "user"),
            ]
        );
    }
}
