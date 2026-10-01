//! The VCS and review RPC handlers, called with wire JSON on temporary repositories.

mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use futures::StreamExt;
use serde_json::{json, Value};
use zc_core::vcs_process::VcsProcess;
use zc_ports::contracts::{AuthSessionId, BackgroundPolicySnapshot, BackgroundScope, ClientActivityReportInput, HostPowerSnapshot, RpcClientId};
use zc_ports::{BackgroundPolicy, BackgroundPolicySubscription};
use zc_rpc::{RpcError, RpcRouter};
use zc_vcs::broadcaster::{fixed_interval, NoAutoPull, VcsStatusBroadcaster};
use zc_vcs::registry::{VcsDriverRegistry, VcsProjectConfig, VcsProvisioningService};
use zc_vcs::rpc::{self, VcsRpcServices};
use zc_vcs::status::{GitStatusService, NoPullRequests};
use zc_vcs::vcs_driver::GitVcsProcessDriver;
use zc_vcs::workflow::{GitWorkflowService, StatusOnlyGitManager};
use zc_vcs::ReviewService;

struct AlwaysRun;

#[async_trait]
impl BackgroundPolicy for AlwaysRun {
    async fn report_client_activity(&self, _: &AuthSessionId, _: &RpcClientId, _: ClientActivityReportInput) {}
    async fn remove_rpc_client(&self, _: &AuthSessionId, _: &RpcClientId) {}
    async fn report_host_power_state(&self, _: HostPowerSnapshot) {}
    async fn snapshot(&self) -> BackgroundPolicySnapshot {
        BackgroundPolicySnapshot::default()
    }
    async fn subscribe(&self) -> BackgroundPolicySubscription {
        BackgroundPolicySubscription {
            latest: BackgroundPolicySnapshot::default(),
            changes: futures::stream::empty().boxed(),
        }
    }
    async fn has_demand(&self, _: &BackgroundScope) -> bool {
        true
    }
    async fn should_run_scope_work(&self, _: &BackgroundScope) -> bool {
        true
    }
    async fn should_run_opportunistic_work(&self) -> bool {
        true
    }
}

/// The full stack the server will wire: driver, registry, status, workflow, broadcaster,
/// provisioning, review.
fn services(workspace: &Tmp) -> (VcsRpcServices, Tmp) {
    let (git, worktrees) = driver();
    let registry = VcsDriverRegistry::new(VcsProjectConfig::new(), Arc::new(GitVcsProcessDriver::new(VcsProcess::default())));
    let status = GitStatusService::new(git.clone(), Arc::new(NoPullRequests));
    let workflow = GitWorkflowService::new(registry.clone(), git.clone(), Arc::new(StatusOnlyGitManager::new(status)));
    let broadcaster = VcsStatusBroadcaster::new(Arc::new(workflow.clone()), Arc::new(AlwaysRun), Arc::new(NoAutoPull));
    let review = ReviewService::new(&workspace.path, &worktrees.path, registry.clone(), git);
    (
        VcsRpcServices {
            workflow,
            broadcaster,
            provisioning: VcsProvisioningService::new(registry),
            review,
            automatic_git_fetch_interval: fixed_interval(Duration::from_secs(3600)),
        },
        worktrees,
    )
}

#[tokio::test]
async fn serves_status_refs_and_ref_mutations_as_wire_json() {
    let workspace = Tmp::new("zc-vcs-rpc-");
    let branch = init_repo_with_commit(&workspace.path);
    write(&workspace.path, "dirty.txt", "x\n");
    let (services, _w) = services(&workspace);
    let cwd = workspace.str();

    let status = rpc::vcs_refresh_status(&services, json!({"cwd": format!("  {cwd} ")})).await.unwrap();
    assert_eq!(status["isRepo"], json!(true));
    assert_eq!(status["refName"], json!(branch));
    assert_eq!(status["hasWorkingTreeChanges"], json!(true));
    assert_eq!(status["workingTree"]["files"][0]["path"], json!("dirty.txt"));
    assert_eq!(status["pr"], Value::Null);
    assert!(status.get("sourceControlProvider").is_none());

    let refs = rpc::vcs_list_refs(&services, json!({"cwd": cwd, "limit": 10})).await.unwrap();
    assert_eq!(
        refs,
        json!({
            "refs": [{"name": branch, "isRemote": false, "current": true, "isDefault": false, "worktreePath": cwd}],
            "isRepo": true,
            "hasPrimaryRemote": false,
            "nextCursor": null,
            "totalCount": 1
        })
    );

    let created = rpc::vcs_create_ref(&services, json!({"cwd": cwd, "refName": "feature/rpc", "switchRef": true}))
        .await
        .unwrap();
    assert_eq!(created, json!({"refName": "feature/rpc"}));
    // Each mutation starts a detached status refresh, whose `git status` may briefly hold the
    // index lock (as in TS); a checkout right behind it can lose that race, so retry.
    let mut switched = Err(RpcError::Interrupt);
    for _ in 0..20 {
        switched = rpc::vcs_switch_ref(&services, json!({"cwd": cwd, "refName": branch})).await;
        if switched.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(switched.unwrap(), json!({"refName": branch}));
    services.broadcaster.refresh_status(cwd).await.unwrap();

    let parent = Tmp::new("zc-vcs-rpc-worktrees-");
    let path = parent.join("wt");
    let worktree = rpc::vcs_create_worktree(&services, json!({"cwd": cwd, "refName": branch, "newRefName": "feature/wt", "path": path}))
        .await
        .unwrap();
    assert_eq!(worktree, json!({"worktree": {"path": path, "refName": "feature/wt"}}));
    let removed = rpc::vcs_remove_worktree(&services, json!({"cwd": cwd, "path": path, "force": true}))
        .await
        .unwrap();
    assert_eq!(removed, Value::Null);
}

#[tokio::test]
async fn streams_vcs_status_and_initializes_repositories() {
    let workspace = Tmp::new("zc-vcs-rpc-");
    let (services, _w) = services(&workspace);
    let cwd = workspace.str();
    let mut stream = rpc::subscribe_vcs_status(&services, json!({"cwd": cwd})).await.unwrap();
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(first["_tag"], json!("snapshot"));
    assert_eq!(first["local"]["isRepo"], json!(false));
    assert_eq!(first["remote"], Value::Null);

    assert_eq!(rpc::vcs_init(&services, json!({"cwd": cwd})).await.unwrap(), Value::Null);
    assert!(workspace.path.join(".git").exists());
    // The detached refresh publishes the new local status to the open stream.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let event = tokio::time::timeout_at(deadline, stream.next())
            .await
            .expect("a status update after vcs.init")
            .unwrap()
            .unwrap();
        let local = match event["_tag"].as_str() {
            Some("snapshot") | Some("localUpdated") => event["local"].clone(),
            _ => continue,
        };
        if local["isRepo"] == json!(true) {
            break;
        }
    }
}

#[tokio::test]
async fn serves_review_previews_and_file_contents() {
    let workspace = Tmp::new("zc-vcs-rpc-");
    init_repo_with_commit(&workspace.path);
    write(&workspace.path, "README.md", "# changed\n");
    let (services, _w) = services(&workspace);
    let cwd = workspace.str();
    let preview = rpc::review_get_diff_preview(&services, json!({"cwd": cwd})).await.unwrap();
    let sources = preview["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0]["id"], json!("working-tree"));
    assert_eq!(
        sources[0]["files"],
        json!([{"path": "README.md", "previousPath": null, "additions": 1, "deletions": 1}])
    );
    assert_eq!(sources[1]["baseRef"], Value::Null);
    let contents = rpc::review_get_diff_file_contents(
        &services,
        json!({
            "cwd": cwd,
            "sourceKind": "working-tree",
            "changeType": "change",
            "baseRef": "HEAD",
            "headRef": null,
            "oldPath": "README.md",
            "newPath": "README.md"
        }),
    )
    .await
    .unwrap();
    assert_eq!(contents, json!({"oldContents": "# test\n", "newContents": "# changed\n"}));
}

#[tokio::test]
async fn fails_with_tagged_errors_and_dies_on_bad_payloads() {
    let workspace = Tmp::new("zc-vcs-rpc-");
    init_repo_with_commit(&workspace.path);
    let (services, _w) = services(&workspace);
    let cwd = workspace.str();

    let Err(RpcError::Fail(error)) = rpc::vcs_pull(&services, json!({"cwd": cwd})).await else {
        panic!("expected a typed failure");
    };
    assert_eq!(error["_tag"], json!("GitCommandError"));
    assert_eq!(error["operation"], json!("GitVcsDriver.pullCurrentBranch"));
    assert_eq!(error["detail"], json!("Current branch has no upstream configured. Push with upstream first."));

    let Err(RpcError::Fail(error)) = rpc::vcs_switch_ref(&services, json!({"cwd": cwd, "refName": "missing-branch"})).await else {
        panic!("expected a typed failure");
    };
    assert_eq!(error["detail"], json!("git checkout failed"));
    assert!(error.get("exitCode").is_some());

    let Err(RpcError::Fail(error)) = rpc::vcs_init(&services, json!({"cwd": cwd, "kind": "unknown"})).await else {
        panic!("expected a typed failure");
    };
    assert_eq!(error["_tag"], json!("VcsUnsupportedOperationError"));

    let outside = Tmp::new("zc-vcs-rpc-outside-");
    let Err(RpcError::Fail(error)) = rpc::review_get_diff_preview(&services, json!({"cwd": outside.str()})).await else {
        panic!("expected a typed failure");
    };
    assert_eq!(error["_tag"], json!("VcsRepositoryDetectionError"));

    assert!(matches!(rpc::vcs_list_refs(&services, json!({"cwd": "  "})).await, Err(RpcError::Die(_))));
    assert!(matches!(
        rpc::vcs_list_refs(&services, json!({"cwd": cwd, "limit": 201})).await,
        Err(RpcError::Die(_))
    ));
    assert!(matches!(rpc::vcs_refresh_status(&services, json!({})).await, Err(RpcError::Die(_))));
}

#[tokio::test]
async fn registers_every_handler_with_its_scope() {
    let workspace = Tmp::new("zc-vcs-rpc-");
    let (services, _w) = services(&workspace);
    let router = rpc::register(RpcRouter::builder(), services).build().unwrap();
    let mut tags: Vec<&str> = router.tags().collect();
    tags.sort();
    let mut expected: Vec<&str> = rpc::METHOD_SCOPES.iter().map(|(tag, _)| *tag).collect();
    expected.sort();
    assert_eq!(tags, expected);
}
