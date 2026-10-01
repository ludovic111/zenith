//! The RPC handlers of this crate, with wire JSON: registration and scopes, the turn diff
//! methods on a real repository and engine, the worktree setup card, and the background
//! policy methods including the socket-close cleanup.

mod common;

use std::sync::Arc;

use common::*;
use serde_json::json;
use zc_checkpoints::background::{BackgroundPolicyService, HostPowerMonitor};
use zc_checkpoints::rpc::{self, BackgroundRpc, CheckpointRpcServices};
use zc_checkpoints::{checkpoint_ref_for_thread_turn, CheckpointDiffQuery, WorktreeSetupTracker};
use zc_contracts::{AuthSessionId, ThreadId, WorktreeSetupStageId};
use zc_rpc::{RpcError, RpcRouter};

async fn services() -> (CheckpointRpcServices, EngineProjections, BackgroundPolicyService) {
    let engine = engine().await;
    let projections = EngineProjections { engine };
    let policy = BackgroundPolicyService::new(Arc::new(HostPowerMonitor::new(None)), MemorySettings::new(json!({})));
    let services = CheckpointRpcServices {
        diff_query: CheckpointDiffQuery::new(Arc::new(projections.clone()), store()),
        worktree_setup: WorktreeSetupTracker::new(),
        background: BackgroundRpc::new(policy.clone()),
    };
    (services, projections, policy)
}

#[tokio::test]
async fn registers_every_method_with_its_scope_and_kind() {
    let (services, _, _) = services().await;
    let router = rpc::register(RpcRouter::builder(), services).build().unwrap();
    let mut tags: Vec<&str> = router.tags().collect();
    tags.sort();
    let mut expected: Vec<&str> = rpc::METHOD_SCOPES.iter().map(|(tag, _)| *tag).collect();
    expected.sort();
    assert_eq!(tags, expected);
    assert_eq!(router.is_stream("subscribeWorktreeSetup"), Some(true));
    assert_eq!(router.is_stream("subscribeBackgroundPolicy"), Some(true));
    assert_eq!(router.is_stream("orchestration.getTurnDiff"), Some(false));
    // The scopes match the generated RPC table.
    for (tag, scope) in rpc::METHOD_SCOPES {
        let spec = zc_contracts::Rpc::from_tag(tag).unwrap().spec();
        assert_eq!(serde_json::to_value(spec.scope).unwrap(), json!(scope), "{tag}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_diffs_come_from_the_checkpoint_refs_and_failures_wrap_their_cause() {
    let (services, projections, _) = services().await;
    let (_dir, repo) = temp_dir();
    create_git_repository(&repo);
    let cwd = repo.to_str().unwrap();
    let engine = &projections.engine;
    dispatch(
        engine,
        json!({"type": "project.create", "commandId": "c1", "projectId": "project-1", "title": "P", "workspaceRoot": cwd,
               "defaultModelSelection": model_selection(), "createdAt": NOW}),
    )
    .await;
    dispatch(
        engine,
        json!({"type": "thread.create", "commandId": "c2", "threadId": "thread-1", "projectId": "project-1", "title": "T",
               "modelSelection": model_selection(), "interactionMode": "default", "runtimeMode": "full-access", "branch": null, "worktreePath": null, "createdAt": NOW}),
    )
    .await;
    let thread = ThreadId::new("thread-1");
    let store = store();
    store.capture_checkpoint(cwd, &checkpoint_ref_for_thread_turn(&thread, 0)).await.unwrap();
    std::fs::write(repo.join("README.md"), "v2\n").unwrap();
    store.capture_checkpoint(cwd, &checkpoint_ref_for_thread_turn(&thread, 1)).await.unwrap();
    dispatch(
        engine,
        json!({"type": "thread.turn.diff.complete", "commandId": "c3", "threadId": "thread-1", "turnId": "turn-1", "completedAt": NOW,
               "checkpointRef": checkpoint_ref_for_thread_turn(&thread, 1), "status": "ready", "files": [], "checkpointTurnCount": 1, "createdAt": NOW}),
    )
    .await;

    let diff = rpc::get_turn_diff(&services, json!({"threadId": "thread-1", "fromTurnCount": 0, "toTurnCount": 1}))
        .await
        .unwrap();
    assert_eq!(diff["fromTurnCount"], json!(0));
    assert!(diff["diff"].as_str().unwrap().contains("-v1\n+v2"));
    let full = rpc::get_full_thread_diff(&services, json!({"threadId": "thread-1", "toTurnCount": 1}))
        .await
        .unwrap();
    assert_eq!(full["diff"], diff["diff"]);

    let error = rpc::get_turn_diff(&services, json!({"threadId": "thread-missing", "fromTurnCount": 0, "toTurnCount": 1}))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        RpcError::Fail(json!({"_tag": "OrchestrationGetTurnDiffError", "message": "Failed to load turn diff",
            "cause": {"name": "CheckpointThreadNotFoundError", "message": "Checkpoint invariant violation in CheckpointDiffQuery.getTurnDiff: Thread 'thread-missing' not found."}}))
    );
    let error = rpc::get_full_thread_diff(&services, json!({"threadId": "thread-1", "toTurnCount": 5}))
        .await
        .unwrap_err();
    let RpcError::Fail(value) = error else { panic!("a typed failure") };
    assert_eq!(value["_tag"], json!("OrchestrationGetFullThreadDiffError"));
    assert_eq!(value["message"], json!("Failed to load full thread diff"));
    assert_eq!(value["cause"]["name"], json!("CheckpointTurnRangeUnavailableError"));
    // An undecodable payload dies with the decode error.
    assert!(matches!(rpc::get_turn_diff(&services, json!({"threadId": 3})).await, Err(RpcError::Die(_))));
}

#[tokio::test]
async fn worktree_setup_cancel_reports_whether_a_setup_was_running() {
    let (services, _, _) = services().await;
    assert_eq!(
        rpc::worktree_setup_cancel(&services, json!({"threadId": "t"})).await.unwrap(),
        json!({"cancelled": false})
    );
    services
        .worktree_setup
        .begin(&ThreadId::new("t"), None, None, &[WorktreeSetupStageId::Agent], None);
    assert_eq!(
        rpc::worktree_setup_cancel(&services, json!({"threadId": "t"})).await.unwrap(),
        json!({"cancelled": false})
    );
}

#[tokio::test]
async fn client_activity_is_dropped_when_its_socket_closes() {
    let (services, _, policy) = services().await;
    let input = decode(
        json!({"clientId": "c", "clientKind": "web", "visible": true, "focused": true, "recentlyInteracted": true,
                              "scopes": [{"type": "thread", "threadId": "t"}], "observedAt": NOW}),
    );
    services.background.report_client_activity(&AuthSessionId::new("s"), 7, input).await;
    let snapshot = serde_json::to_value(policy.snapshot().await).unwrap();
    assert_eq!(snapshot["activeScopeKeys"], json!(["thread:t"]));
    assert_eq!(snapshot["leases"][0]["rpcClientId"], json!(7));
    services.background.connection_closed(7).await;
    assert_eq!(serde_json::to_value(policy.snapshot().await).unwrap()["leases"], json!([]));
}
