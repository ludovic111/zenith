//! Ports of `vcs/VcsStatusBroadcaster.test.ts` with a scripted workflow and background policy.

mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::StreamExt;
use zc_ports::contracts::{AuthSessionId, BackgroundPolicySnapshot, BackgroundScope, ClientActivityReportInput, HostPowerSnapshot, RpcClientId};
use zc_ports::{BackgroundPolicy, BackgroundPolicySubscription};
use zc_vcs::broadcaster::{fixed_interval, AutoPullPolicy, NoAutoPull, StatusWorkflow, VcsStatusBroadcaster};
use zc_vcs::contracts::*;
use zc_vcs::errors::{GitCommandError, GitManagerError, GitManagerServiceError};
use zc_vcs::status::RemoteStatusOptions;

fn base_local() -> VcsStatusLocalResult {
    VcsStatusLocalResult {
        is_repo: true,
        source_control_provider: Some(SourceControlProviderInfo {
            kind: SourceControlProviderKind::Github,
            name: "GitHub".into(),
            base_url: "https://github.com".into(),
        }),
        has_primary_remote: true,
        is_default_ref: false,
        ref_name: Some("feature/status-broadcast".into()),
        has_working_tree_changes: false,
        working_tree: WorkingTree::default(),
    }
}

fn base_remote() -> VcsStatusRemoteResult {
    VcsStatusRemoteResult {
        has_upstream: true,
        ahead_count: 0,
        behind_count: 0,
        ahead_of_default_count: None,
        pr: None,
    }
}

fn remote_with_pr() -> VcsStatusRemoteResult {
    VcsStatusRemoteResult {
        pr: Some(VcsStatusChangeRequest {
            number: 2978,
            title: "[codex] Rewrite client connection architecture".into(),
            url: "https://github.com/pingdotgg/t3code/pull/2978".into(),
            base_ref: "main".into(),
            head_ref: "codex/connection-state-audit".into(),
            state: ChangeRequestState::Open,
            is_draft: None,
            updated_at: Nullable::Absent,
        }),
        ..base_remote()
    }
}

type RemoteHook = Box<dyn Fn(usize) -> Option<BoxFuture<'static, Result<Option<VcsStatusRemoteResult>, GitManagerServiceError>>> + Send + Sync>;

#[derive(Default)]
struct Counters {
    local_calls: usize,
    remote_calls: usize,
    local_invalidations: usize,
    remote_invalidations: usize,
    refresh_upstream: Vec<bool>,
    seen_cwds: Vec<String>,
    pulls: usize,
}

struct FakeWorkflow {
    local: Mutex<VcsStatusLocalResult>,
    remote: Mutex<Option<VcsStatusRemoteResult>>,
    fail_remote: AtomicBool,
    counters: Mutex<Counters>,
    remote_hook: Option<RemoteHook>,
}

impl FakeWorkflow {
    fn new() -> Self {
        Self {
            local: Mutex::new(base_local()),
            remote: Mutex::new(Some(base_remote())),
            fail_remote: AtomicBool::new(false),
            counters: Mutex::new(Counters::default()),
            remote_hook: None,
        }
    }

    fn with_hook(hook: RemoteHook) -> Self {
        Self {
            remote_hook: Some(hook),
            ..Self::new()
        }
    }

    fn counts(&self) -> (usize, usize, usize, usize) {
        let c = self.counters.lock().unwrap();
        (c.local_calls, c.remote_calls, c.local_invalidations, c.remote_invalidations)
    }
}

#[async_trait]
impl StatusWorkflow for FakeWorkflow {
    async fn local_status(&self, cwd: &str) -> Result<VcsStatusLocalResult, GitManagerServiceError> {
        let mut c = self.counters.lock().unwrap();
        c.local_calls += 1;
        c.seen_cwds.push(cwd.to_owned());
        Ok(self.local.lock().unwrap().clone())
    }

    async fn remote_status(&self, cwd: &str, options: RemoteStatusOptions) -> Result<Option<VcsStatusRemoteResult>, GitManagerServiceError> {
        let call = {
            let mut c = self.counters.lock().unwrap();
            c.remote_calls += 1;
            c.refresh_upstream.push(options.refresh_upstream);
            c.seen_cwds.push(cwd.to_owned());
            c.remote_calls
        };
        if let Some(hook) = &self.remote_hook {
            if let Some(future) = hook(call) {
                return future.await;
            }
        }
        if self.fail_remote.load(Ordering::SeqCst) {
            return Err(GitManagerError::new("VcsStatusBroadcaster.test", "/repo", "remote status failed").into());
        }
        Ok(self.remote.lock().unwrap().clone())
    }

    async fn invalidate_local_status(&self, _cwd: &str) {
        self.counters.lock().unwrap().local_invalidations += 1;
    }

    async fn invalidate_remote_status(&self, _cwd: &str) {
        self.counters.lock().unwrap().remote_invalidations += 1;
    }

    async fn invalidate_status(&self, _cwd: &str) {
        let mut c = self.counters.lock().unwrap();
        c.local_invalidations += 1;
        c.remote_invalidations += 1;
    }

    async fn pull_current_branch(&self, _cwd: &str) -> Result<VcsPullResult, GitCommandError> {
        self.counters.lock().unwrap().pulls += 1;
        let mut remote = self.remote.lock().unwrap();
        if let Some(remote) = remote.as_mut() {
            remote.behind_count = 0;
        }
        Ok(VcsPullResult {
            status: VcsPullStatus::Pulled,
            ref_name: "main".into(),
            upstream_ref: Some("origin/main".into()),
        })
    }
}

/// `makeBackgroundPolicyLayer(() => enabled)`.
struct Policy(bool);

#[async_trait]
impl BackgroundPolicy for Policy {
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
        self.0
    }
    async fn should_run_opportunistic_work(&self) -> bool {
        true
    }
}

fn broadcaster(workflow: Arc<FakeWorkflow>, policy: bool) -> VcsStatusBroadcaster {
    VcsStatusBroadcaster::new(workflow, Arc::new(Policy(policy)), Arc::new(NoAutoPull))
}

fn merged(local: VcsStatusLocalResult, remote: VcsStatusRemoteResult) -> VcsStatusResult {
    VcsStatusResult::merge(local, Some(remote))
}

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn reuses_the_cached_status_across_repeated_reads() {
    let workflow = Arc::new(FakeWorkflow::new());
    let broadcaster = broadcaster(workflow.clone(), true);
    let first = broadcaster.get_status("/repo").await.unwrap();
    let second = broadcaster.get_status("/repo").await.unwrap();
    assert_eq!(first, merged(base_local(), base_remote()));
    assert_eq!(second, first);
    assert_eq!(workflow.counts(), (1, 1, 0, 0));
}

#[tokio::test]
async fn refreshes_a_loaded_cwd_without_reusing_a_previous_branch_pr() {
    let workflow = Arc::new(FakeWorkflow::new());
    let broadcaster = broadcaster(workflow.clone(), true);
    // Nobody loaded this cwd yet: no host request is spent.
    assert_eq!(broadcaster.refresh_pull_request_status("/repo").await.unwrap(), None);
    assert_eq!(workflow.counts().1, 0);
    broadcaster.get_status("/repo").await.unwrap();
    assert_eq!(workflow.counts().1, 1);
    *workflow.remote.lock().unwrap() = Some(remote_with_pr());
    let refreshed = broadcaster.refresh_pull_request_status("/repo").await.unwrap();
    assert_eq!(refreshed, Some(remote_with_pr()));
    assert_eq!(workflow.counts().1, 2);
    assert_eq!(workflow.counts().3, 0);
    // The retry asks for the missing PR without fetching.
    let options = workflow.counters.lock().unwrap().refresh_upstream.clone();
    assert_eq!(options.last(), Some(&false));

    *workflow.local.lock().unwrap() = VcsStatusLocalResult {
        ref_name: Some("feature/next".into()),
        ..base_local()
    };
    *workflow.remote.lock().unwrap() = Some(base_remote());
    broadcaster.refresh_local_status("/repo").await.unwrap();
    let refreshed = broadcaster.refresh_pull_request_status("/repo").await.unwrap();
    assert_eq!(refreshed, Some(base_remote()));
    assert_eq!(workflow.counts().1, 3);
}

#[tokio::test]
async fn a_poll_started_before_the_turn_end_refresh_cannot_overwrite_its_pr() {
    let release = Arc::new(tokio::sync::Notify::new());
    let started = Arc::new(tokio::sync::Notify::new());
    let (release_hook, started_hook) = (release.clone(), started.clone());
    let workflow = Arc::new(FakeWorkflow::with_hook(Box::new(move |call| {
        let (release, started) = (release_hook.clone(), started_hook.clone());
        Some(Box::pin(async move {
            match call {
                1 => Ok(Some(base_remote())),
                2 => {
                    // Hold an older empty response while the turn-end refresh queues.
                    started.notify_one();
                    release.notified().await;
                    Ok(Some(base_remote()))
                }
                _ => Ok(Some(remote_with_pr())),
            }
        }))
    })));
    let broadcaster = broadcaster(workflow, true);
    broadcaster.get_status("/repo").await.unwrap();
    let poll = {
        let b = broadcaster.clone();
        tokio::spawn(async move { b.refresh_status("/repo").await })
    };
    started.notified().await;
    let refresh = {
        let b = broadcaster.clone();
        tokio::spawn(async move { b.refresh_pull_request_status("/repo").await })
    };
    settle().await;
    release.notify_one();
    poll.await.unwrap().unwrap();
    assert_eq!(refresh.await.unwrap().unwrap(), Some(remote_with_pr()));
    assert_eq!(broadcaster.get_status("/repo").await.unwrap().remote.pr, remote_with_pr().pr);
}

#[tokio::test]
async fn an_initial_status_read_cannot_overwrite_an_explicit_refresh() {
    let release = Arc::new(tokio::sync::Notify::new());
    let started = Arc::new(tokio::sync::Notify::new());
    let (release_hook, started_hook) = (release.clone(), started.clone());
    let workflow = Arc::new(FakeWorkflow::with_hook(Box::new(move |call| {
        let (release, started) = (release_hook.clone(), started_hook.clone());
        Some(Box::pin(async move {
            if call == 1 {
                started.notify_one();
                release.notified().await;
                return Ok(Some(base_remote()));
            }
            Ok(Some(remote_with_pr()))
        }))
    })));
    let broadcaster = broadcaster(workflow, true);
    let initial = {
        let b = broadcaster.clone();
        tokio::spawn(async move { b.get_status("/repo").await })
    };
    started.notified().await;
    let refresh = {
        let b = broadcaster.clone();
        tokio::spawn(async move { b.refresh_status("/repo").await })
    };
    settle().await;
    release.notify_one();
    initial.await.unwrap().unwrap();
    refresh.await.unwrap().unwrap();
    assert_eq!(broadcaster.get_status("/repo").await.unwrap().remote.pr, remote_with_pr().pr);
}

#[tokio::test]
async fn turn_end_refresh_skips_a_loaded_cwd_when_background_policy_pauses_it() {
    let workflow = Arc::new(FakeWorkflow::new());
    let broadcaster = broadcaster(workflow.clone(), false);
    broadcaster.get_status("/repo").await.unwrap();
    assert_eq!(broadcaster.refresh_pull_request_status("/repo").await.unwrap(), None);
    assert_eq!(workflow.counts().1, 1);
    assert_eq!(workflow.counts().3, 0);
}

#[tokio::test]
async fn refreshes_the_cached_snapshot_after_explicit_invalidation() {
    let workflow = Arc::new(FakeWorkflow::new());
    let broadcaster = broadcaster(workflow.clone(), true);
    let initial = broadcaster.get_status("/repo").await.unwrap();
    let local = VcsStatusLocalResult {
        ref_name: Some("feature/updated-status".into()),
        ..base_local()
    };
    let remote = VcsStatusRemoteResult {
        ahead_count: 2,
        ..base_remote()
    };
    *workflow.local.lock().unwrap() = local.clone();
    *workflow.remote.lock().unwrap() = Some(remote.clone());
    let refreshed = broadcaster.refresh_status("/repo").await.unwrap();
    let cached = broadcaster.get_status("/repo").await.unwrap();
    assert_eq!(initial, merged(base_local(), base_remote()));
    assert_eq!(refreshed, merged(local.clone(), remote.clone()));
    assert_eq!(cached, merged(local, remote));
    assert_eq!(workflow.counts(), (2, 2, 1, 1));
}

#[tokio::test]
async fn keeps_the_cached_snapshot_unchanged_when_a_refresh_branch_fails() {
    let workflow = Arc::new(FakeWorkflow::new());
    let broadcaster = broadcaster(workflow.clone(), true);
    broadcaster.get_status("/repo").await.unwrap();
    *workflow.local.lock().unwrap() = VcsStatusLocalResult {
        ref_name: Some("feature/partial-refresh".into()),
        ..base_local()
    };
    workflow.fail_remote.store(true, Ordering::SeqCst);
    assert!(broadcaster.refresh_status("/repo").await.is_err());
    assert_eq!(broadcaster.get_status("/repo").await.unwrap(), merged(base_local(), base_remote()));
}

#[tokio::test]
async fn refreshes_only_the_cached_local_snapshot_when_requested() {
    let workflow = Arc::new(FakeWorkflow::new());
    let broadcaster = broadcaster(workflow.clone(), true);
    let initial = broadcaster.get_status("/repo").await.unwrap();
    let local = VcsStatusLocalResult {
        ref_name: Some("feature/local-only-refresh".into()),
        has_working_tree_changes: true,
        ..base_local()
    };
    *workflow.local.lock().unwrap() = local.clone();
    let refreshed = broadcaster.refresh_local_status("/repo").await.unwrap();
    let cached = broadcaster.get_status("/repo").await.unwrap();
    assert_eq!(initial, merged(base_local(), base_remote()));
    assert_eq!(refreshed, local);
    assert_eq!(cached, merged(local, base_remote()));
    assert_eq!(workflow.counts(), (2, 1, 1, 0));
}

#[tokio::test]
async fn normalizes_symlinked_cwds_before_cache_lookup_and_workflow_calls() {
    let real = common::Tmp::new("t3-vcs-status-real-");
    let parent = common::Tmp::new("t3-vcs-status-link-");
    let link = parent.path.join("repo-link");
    std::os::unix::fs::symlink(&real.path, &link).unwrap();
    let workflow = Arc::new(FakeWorkflow::new());
    let broadcaster = broadcaster(workflow.clone(), true);
    broadcaster.get_status(link.to_str().unwrap()).await.unwrap();
    broadcaster.get_status(real.str()).await.unwrap();
    assert_eq!(workflow.counters.lock().unwrap().seen_cwds, vec![real.str().to_owned(), real.str().to_owned()]);
    assert_eq!(workflow.counts().0, 1);
    assert_eq!(workflow.counts().1, 1);
}

#[tokio::test]
async fn streams_a_local_snapshot_first_and_remote_updates_later() {
    let workflow = Arc::new(FakeWorkflow::new());
    let broadcaster = broadcaster(workflow.clone(), true);
    // A long interval: the immediate initial poller load races the explicit refresh below,
    // and whichever publishes first, the remote part changes exactly once.
    let mut stream = broadcaster
        .stream_status("/repo", Some(fixed_interval(Duration::from_secs(3600))))
        .await
        .unwrap();
    let snapshot = stream.next().await.unwrap();
    assert_eq!(
        serde_json::to_value(&snapshot).unwrap(),
        serde_json::json!({
            "_tag": "snapshot",
            "local": serde_json::to_value(base_local()).unwrap(),
            "remote": null
        })
    );
    broadcaster.refresh_status("/repo").await.unwrap();
    let mut remote_updated = None;
    while let Some(event) = stream.next().await {
        match event {
            VcsStatusStreamEvent::RemoteUpdated { .. } | VcsStatusStreamEvent::Snapshot { .. } => {
                remote_updated = Some(event);
                break;
            }
            VcsStatusStreamEvent::LocalUpdated { .. } => {}
        }
    }
    let remote_updated = remote_updated.unwrap();
    match remote_updated {
        VcsStatusStreamEvent::RemoteUpdated { remote } => assert_eq!(remote, Some(base_remote())),
        VcsStatusStreamEvent::Snapshot { remote, .. } => assert_eq!(remote, Some(base_remote())),
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn loads_remote_status_once_when_periodic_refreshes_are_disabled() {
    let workflow = Arc::new(FakeWorkflow::new());
    *workflow.remote.lock().unwrap() = Some(remote_with_pr());
    let broadcaster = broadcaster(workflow.clone(), true);
    let mut stream = broadcaster.stream_status("/repo", Some(fixed_interval(Duration::ZERO))).await.unwrap();
    let snapshot = stream.next().await.unwrap();
    assert!(matches!(snapshot, VcsStatusStreamEvent::Snapshot { remote: None, .. }));
    let updated = stream.next().await.unwrap();
    assert_eq!(
        updated,
        VcsStatusStreamEvent::RemoteUpdated {
            remote: Some(remote_with_pr())
        }
    );
    assert_eq!(workflow.counts().1, 1);
    assert_eq!(workflow.counts().3, 0);
    assert_eq!(workflow.counters.lock().unwrap().refresh_upstream, vec![false]);
    tokio::time::advance(Duration::from_secs(120)).await;
    settle().await;
    assert_eq!(workflow.counts().1, 1);
    assert_eq!(workflow.counts().3, 0);
    drop(stream);
    assert_eq!(broadcaster.active_poller_count(), 0);
}

#[tokio::test(start_paused = true)]
async fn retries_the_initial_remote_load_when_periodic_refreshes_are_disabled() {
    let workflow = Arc::new(FakeWorkflow::with_hook(Box::new(|call| {
        Some(Box::pin(async move {
            if call == 1 {
                Err(GitManagerError::new(
                    "VcsStatusBroadcaster.test",
                    "/private/user/workspace/repo",
                    "private initial remote status failure",
                )
                .into())
            } else {
                Ok(Some(remote_with_pr()))
            }
        }))
    })));
    let broadcaster = broadcaster(workflow.clone(), true);
    let mut stream = broadcaster
        .stream_status("/private/user/workspace/repo", Some(fixed_interval(Duration::ZERO)))
        .await
        .unwrap();
    stream.next().await.unwrap();
    settle().await;
    assert_eq!(workflow.counts().1, 1);
    tokio::time::advance(Duration::from_secs(29)).await;
    settle().await;
    assert_eq!(workflow.counts().1, 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    let updated = stream.next().await.unwrap();
    assert_eq!(
        updated,
        VcsStatusStreamEvent::RemoteUpdated {
            remote: Some(remote_with_pr())
        }
    );
    assert_eq!(workflow.counts().1, 2);
    assert_eq!(workflow.counts().3, 0);
    assert_eq!(workflow.counters.lock().unwrap().refresh_upstream, vec![false, false]);
}

#[tokio::test(start_paused = true)]
async fn delays_automatic_refresh_when_a_cached_remote_snapshot_is_available() {
    let workflow = Arc::new(FakeWorkflow::new());
    let broadcaster = broadcaster(workflow.clone(), true);
    broadcaster.get_status("/repo").await.unwrap();
    let mut stream = broadcaster.stream_status("/repo", Some(fixed_interval(Duration::from_secs(60)))).await.unwrap();
    let snapshot = stream.next().await.unwrap();
    assert!(matches!(snapshot, VcsStatusStreamEvent::Snapshot { remote: Some(_), .. }));
    settle().await;
    assert_eq!(workflow.counts().1, 1);
    tokio::time::advance(Duration::from_secs(59)).await;
    settle().await;
    assert_eq!(workflow.counts().1, 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(workflow.counts().1, 2);
    assert_eq!(workflow.counts().3, 1);
    assert_eq!(workflow.counters.lock().unwrap().refresh_upstream.last(), Some(&true));
}

#[tokio::test]
async fn does_not_start_automatic_refreshes_without_foreground_demand() {
    let workflow = Arc::new(FakeWorkflow::new());
    let broadcaster = broadcaster(workflow.clone(), false);
    let mut stream = broadcaster.stream_status("/repo", Some(fixed_interval(Duration::from_secs(1)))).await.unwrap();
    assert!(matches!(stream.next().await, Some(VcsStatusStreamEvent::Snapshot { .. })));
    drop(stream);
    settle().await;
    assert_eq!(workflow.counts().1, 0);
    assert_eq!(workflow.counts().3, 0);
}

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn stops_the_remote_poller_after_the_last_subscriber_disconnects() {
    let interrupted = Arc::new(AtomicBool::new(false));
    let started = Arc::new(AtomicUsize::new(0));
    let (flag, counter) = (interrupted.clone(), started.clone());
    let workflow = Arc::new(FakeWorkflow::with_hook(Box::new(move |_| {
        let (flag, counter) = (flag.clone(), counter.clone());
        Some(Box::pin(async move {
            let _guard = DropFlag(flag);
            counter.fetch_add(1, Ordering::SeqCst);
            futures::future::pending::<()>().await;
            unreachable!()
        }))
    })));
    let broadcaster = broadcaster(workflow.clone(), true);
    let mut first = broadcaster.stream_status("/repo", None).await.unwrap();
    let mut second = broadcaster.stream_status("/repo", None).await.unwrap();
    first.next().await.unwrap();
    second.next().await.unwrap();
    settle().await;
    assert_eq!(started.load(Ordering::SeqCst), 1);
    assert_eq!(broadcaster.active_poller_count(), 1);
    drop(first);
    settle().await;
    assert!(!interrupted.load(Ordering::SeqCst));
    drop(second);
    settle().await;
    assert!(interrupted.load(Ordering::SeqCst));
    assert_eq!(broadcaster.active_poller_count(), 0);
}

struct RootPolicy(Mutex<String>);

#[async_trait]
impl AutoPullPolicy for RootPolicy {
    async fn is_enabled(&self, cwd: &str) -> bool {
        *self.0.lock().unwrap() == cwd
    }
}

#[tokio::test]
async fn automatically_pulls_an_enabled_clean_default_branch_that_is_behind() {
    let real = common::Tmp::new("t3-vcs-auto-pull-real-");
    let parent = common::Tmp::new("t3-vcs-auto-pull-link-");
    let link = parent.path.join("repo-link");
    std::os::unix::fs::symlink(&real.path, &link).unwrap();
    let workflow = Arc::new(FakeWorkflow::new());
    *workflow.local.lock().unwrap() = VcsStatusLocalResult {
        is_default_ref: true,
        ref_name: Some("main".into()),
        ..base_local()
    };
    *workflow.remote.lock().unwrap() = Some(VcsStatusRemoteResult {
        behind_count: 2,
        ..base_remote()
    });
    let policy = Arc::new(RootPolicy(Mutex::new(link.to_string_lossy().into_owned())));
    let broadcaster = VcsStatusBroadcaster::new(workflow.clone(), Arc::new(Policy(true)), policy);
    let status = broadcaster.refresh_status(link.to_str().unwrap()).await.unwrap();
    assert_eq!(workflow.counters.lock().unwrap().pulls, 1);
    assert_eq!(status.remote.behind_count, 0);

    // A dirty tree is never pulled.
    *workflow.remote.lock().unwrap() = Some(VcsStatusRemoteResult {
        behind_count: 1,
        ..base_remote()
    });
    workflow.local.lock().unwrap().has_working_tree_changes = true;
    broadcaster.refresh_status(link.to_str().unwrap()).await.unwrap();
    assert_eq!(workflow.counters.lock().unwrap().pulls, 1);
}

#[test]
fn ports_the_status_refresher_trait() {
    fn assert_port<T: zc_ports::VcsStatusRefresher>() {}
    assert_port::<VcsStatusBroadcaster>();
}
