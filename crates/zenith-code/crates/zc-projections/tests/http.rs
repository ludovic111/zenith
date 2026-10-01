//! The orchestration HTTP routes (`orchestration/http.ts`): scopes, status codes and bodies.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use zc_db::repos::event_store::{self, NewEvent};
use zc_db::Db;
use zc_http::EnvironmentError;
use zc_ports::DispatchResult;
use zc_projections::http::{router, EnvironmentAuth, HttpDispatch, HttpDispatchError};
use zc_projections::{NoRepositoryIdentities, NoThreadLiveState, ProjectionPipeline, ProjectionSnapshotQuery};

const NOW: &str = "2026-01-01T00:00:00.000Z";

/// Scopes from an `x-test-scopes` header; no header is a missing credential.
struct HeaderAuth;

#[async_trait]
impl EnvironmentAuth for HeaderAuth {
    async fn authenticate(&self, parts: &Parts) -> Result<Vec<String>, EnvironmentError> {
        let scopes = parts.headers.get("x-test-scopes").ok_or_else(EnvironmentError::missing_credential)?;
        Ok(scopes.to_str().unwrap().split(',').map(str::to_owned).collect())
    }
}

struct FakeDispatch;

#[async_trait]
impl HttpDispatch for FakeDispatch {
    async fn dispatch(&self, payload: Value) -> Result<DispatchResult, HttpDispatchError> {
        match payload["type"].as_str() {
            Some("thread.delete") => Err(HttpDispatchError::Failed("no such thread".into())),
            Some("thread.archive") => Err(HttpDispatchError::InvalidCommand),
            _ => Ok(DispatchResult { sequence: 42 }),
        }
    }
}

fn seeded_db() -> Db {
    let db = Db::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let pipeline = ProjectionPipeline::new(dir.path());
    let events = vec![
        json!({"type": "project.created", "aggregateKind": "project", "aggregateId": "p1", "payload": {
            "projectId": "p1", "title": "Project", "workspaceRoot": "/work/p1", "defaultModelSelection": null,
            "scripts": [], "createdAt": NOW, "updatedAt": NOW,
        }}),
        json!({"type": "thread.created", "aggregateKind": "thread", "aggregateId": "t1", "payload": {
            "threadId": "t1", "projectId": "p1", "title": "Thread", "modelSelection": {"instanceId": "codex", "model": "m"},
            "runtimeMode": "full-access", "branch": null, "worktreePath": null, "createdAt": NOW, "updatedAt": NOW,
        }}),
        json!({"type": "thread.message-sent", "aggregateKind": "thread", "aggregateId": "t1", "payload": {
            "threadId": "t1", "messageId": "m1", "role": "reasoning", "text": "thinking", "turnId": null,
            "streaming": false, "createdAt": NOW, "updatedAt": NOW,
        }}),
    ];
    db.call_blocking(move |conn| {
        for (n, event) in events.into_iter().enumerate() {
            let mut event = event;
            event["eventId"] = json!(format!("evt-{n}"));
            event["occurredAt"] = json!(NOW);
            event["commandId"] = json!(format!("cmd-{n}"));
            event["causationEventId"] = Value::Null;
            event["correlationId"] = Value::Null;
            event["metadata"] = json!({});
            let event: NewEvent = serde_json::from_value(event).unwrap();
            let stored = event_store::append(conn, &event)?;
            pipeline.project_persisted_deferred(conn, &stored)?;
        }
        Ok(())
    })
    .unwrap();
    db
}

async fn call(method: &str, uri: &str, scopes: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
    let reads = Arc::new(ProjectionSnapshotQuery::new(
        tokio::task::block_in_place(seeded_db),
        Arc::new(NoRepositoryIdentities),
        Arc::new(NoThreadLiveState),
    ));
    let app = router(reads, Arc::new(FakeDispatch), Arc::new(HeaderAuth));
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(scopes) = scopes {
        request = request.header("x-test-scopes", scopes);
    }
    let request = request
        .header("content-type", "application/json")
        .body(body.map(|body| Body::from(body.to_string())).unwrap_or_else(Body::empty))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

const READ: Option<&str> = Some("orchestration:read");

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_routes_answer_with_the_read_models() {
    let (status, body) = call("GET", "/api/orchestration/shell", READ, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["snapshotSequence"], 3);
    assert_eq!(body["threads"][0]["id"], "t1");

    let (status, body) = call("GET", "/api/orchestration/snapshot", READ, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["threads"][0]["messages"], json!([]), "the command read model has no bodies");

    let (status, body) = call("GET", "/api/orchestration/threads/t1", READ, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["thread"]["messages"][0]["role"], "system", "reasoning is relabelled by default");
    assert!(body.get("page").is_none());

    let (status, body) = call("GET", "/api/orchestration/threads/t1?reasoningMessages=true&turnLimit=2", READ, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["thread"]["messages"][0]["role"], "reasoning");
    assert_eq!(body["page"]["hasMore"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_routes_check_scopes_queries_and_existence() {
    let (status, body) = call("GET", "/api/orchestration/shell", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["reason"], "missing_credential");

    let (status, body) = call("GET", "/api/orchestration/shell", Some("orchestration:operate"), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["_tag"], "EnvironmentScopeRequiredError");
    assert_eq!(body["requiredScope"], "orchestration:read");

    let (status, body) = call("GET", "/api/orchestration/threads/nope", READ, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["reason"], "thread_not_found");

    let (status, body) = call("GET", "/api/orchestration/threads/t1?turnLimit=0", READ, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["_tag"], "HttpApiDecodeError");
    // The middleware runs before the request is decoded, the scope check after.
    let (status, _) = call("GET", "/api/orchestration/threads/t1?turnLimit=0", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call("GET", "/api/orchestration/threads/t1?turnLimit=0", Some("other"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread")]
async fn dispatch_validates_authorizes_and_maps_failures() {
    let operate = Some("orchestration:operate");
    let command = |kind: &str| json!({"type": kind, "commandId": "cmd-x", "threadId": "t1"});

    let (status, body) = call("POST", "/api/orchestration/dispatch", operate, Some(command("thread.unpin"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"sequence": 42}));

    let (status, _) = call("POST", "/api/orchestration/dispatch", READ, Some(command("thread.unpin"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = call("POST", "/api/orchestration/dispatch", operate, Some(json!({"type": "not.a.command"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, body) = call("POST", "/api/orchestration/dispatch", operate, Some(command("thread.archive"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["reason"], "invalid_command");

    let (status, body) = call(
        "POST",
        "/api/orchestration/dispatch",
        operate,
        Some(json!({"type": "thread.delete", "commandId": "cmd-y", "threadId": "t1"})),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["reason"], "orchestration_dispatch_failed");
}
