//! Ports of `background/BackgroundPolicy.test.ts` and `background/HostPowerMonitor.test.ts`.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::*;
use futures::StreamExt;
use serde_json::{json, Value};
use zc_checkpoints::background::policy::MAX_CLIENT_ACTIVITY_LEASES_PER_RPC_CLIENT;
use zc_checkpoints::background::{BackgroundPolicyService, HostPower, HostPowerMonitor};
use zc_contracts::{AuthSessionId, BackgroundBooleanState, BackgroundScope, ClientActivityReportInput, DateTimeUtc, HostPowerSnapshot, RpcClientId};
use zc_core::pubsub::PubSub;
use zc_ports::EventStream;

const TEST_NOW: &str = "2026-05-13T00:00:00.000Z";

fn nominal() -> Value {
    json!({"source": "unknown", "idle": "unknown", "idleSeconds": null, "locked": "unknown", "suspended": false, "onBattery": "unknown",
           "lowPowerMode": "unknown", "thermalState": "unknown", "stale": true, "updatedAt": TEST_NOW})
}

fn power(overrides: Value) -> HostPowerSnapshot {
    let mut value = nominal();
    value.as_object_mut().unwrap().extend(overrides.as_object().unwrap().clone());
    decode(value)
}

fn report(overrides: Value) -> ClientActivityReportInput {
    let mut value = json!({"clientId": "client-1", "clientKind": "web", "visible": true, "focused": true, "recentlyInteracted": true,
                           "scopes": [{"type": "vcs-status", "cwd": "/repo"}], "ttlMs": 45_000, "observedAt": TEST_NOW});
    value.as_object_mut().unwrap().extend(overrides.as_object().unwrap().clone());
    decode(value)
}

fn scope(cwd: &str) -> BackgroundScope {
    decode(json!({"type": "vcs-status", "cwd": cwd}))
}

/// The test host monitor: a fixed snapshot (replaced by `report`), optionally slow on its
/// first read.
struct TestHost {
    snapshot: Mutex<HostPowerSnapshot>,
    changes: PubSub<HostPowerSnapshot>,
    reads: AtomicUsize,
    first_read_gate: Option<Arc<tokio::sync::Semaphore>>,
    first_read_started: tokio::sync::Notify,
}

impl TestHost {
    fn new(snapshot: HostPowerSnapshot) -> Arc<Self> {
        Arc::new(Self {
            snapshot: Mutex::new(snapshot),
            changes: PubSub::new(),
            reads: AtomicUsize::new(0),
            first_read_gate: None,
            first_read_started: tokio::sync::Notify::new(),
        })
    }
}

#[async_trait]
impl HostPower for TestHost {
    async fn snapshot(&self) -> HostPowerSnapshot {
        if self.reads.fetch_add(1, Ordering::SeqCst) == 0 {
            if let Some(gate) = &self.first_read_gate {
                self.first_read_started.notify_one();
                let _ = gate.acquire().await;
            }
        }
        self.snapshot.lock().unwrap().clone()
    }
    async fn report(&self, snapshot: HostPowerSnapshot) {
        *self.snapshot.lock().unwrap() = snapshot.clone();
        self.changes.publish(snapshot);
    }
    fn subscribe_changes(&self) -> EventStream<HostPowerSnapshot> {
        self.changes.subscribe().boxed()
    }
}

fn clock() -> zc_checkpoints::background::policy::Clock {
    let now = DateTimeUtc::parse(TEST_NOW).unwrap();
    Arc::new(move || now)
}

fn policy(host: HostPowerSnapshot, settings: Value) -> BackgroundPolicyService {
    BackgroundPolicyService::with_clock(TestHost::new(host), MemorySettings::new(settings), clock())
}

fn session(id: &str) -> AuthSessionId {
    AuthSessionId::new(id)
}

#[tokio::test]
async fn records_foreground_scoped_client_demand() {
    let policy = policy(power(json!({})), json!({}));
    policy.report_client_activity(&session("session-1"), RpcClientId(1), report(json!({}))).await;
    let snapshot = policy.snapshot().await;
    assert_eq!(snapshot.active_foreground_lease_count.get(), 1.0);
    assert_eq!(snapshot.active_scope_keys, vec!["vcs-status:/repo".to_owned()]);
    assert!(snapshot.should_run_opportunistic_work);
    assert!(policy.has_demand(&scope("/repo")).await);
    assert!(!policy.has_demand(&scope("/other")).await);
    assert!(policy.should_run_scope_work(&scope("/repo")).await);
    assert!(!policy.should_run_scope_work(&scope("/other")).await);
    // The wire form of the snapshot.
    let wire = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(wire["leases"][0]["expiresAt"], json!("2026-05-13T00:00:45.000Z"));
    assert_eq!(wire["leases"][0]["rpcClientId"], json!(1));
    assert_eq!(wire["activeForegroundLeaseCount"], json!(1));
}

#[tokio::test]
async fn removes_all_leases_for_a_disconnected_websocket_connection() {
    let policy = policy(power(json!({})), json!({}));
    policy.report_client_activity(&session("session-1"), RpcClientId(1), report(json!({}))).await;
    policy.remove_rpc_client(&session("session-1"), RpcClientId(1)).await;
    let snapshot = policy.snapshot().await;
    assert_eq!(snapshot.active_foreground_lease_count.get(), 0.0);
    assert!(snapshot.active_scope_keys.is_empty());
    assert!(!snapshot.should_run_opportunistic_work);
}

#[tokio::test]
async fn keeps_leases_from_another_session_when_rpc_client_ids_are_reused() {
    let policy = policy(power(json!({})), json!({}));
    policy
        .report_client_activity(&session("session-1"), RpcClientId(1), report(json!({"clientId": "client-1"})))
        .await;
    policy
        .report_client_activity(&session("session-2"), RpcClientId(1), report(json!({"clientId": "client-2"})))
        .await;
    policy.remove_rpc_client(&session("session-1"), RpcClientId(1)).await;
    let snapshot = policy.snapshot().await;
    assert_eq!(snapshot.active_foreground_lease_count.get(), 1.0);
    assert_eq!(snapshot.leases[0].session_id.as_str(), "session-2");
    assert_eq!(snapshot.leases[0].client_id, "client-2");
}

#[tokio::test(flavor = "multi_thread")]
async fn serializes_lease_mutation_publications() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let host = Arc::new(TestHost {
        snapshot: Mutex::new(power(json!({}))),
        changes: PubSub::new(),
        reads: AtomicUsize::new(0),
        first_read_gate: Some(gate.clone()),
        first_read_started: tokio::sync::Notify::new(),
    });
    let policy = BackgroundPolicyService::with_clock(host.clone(), MemorySettings::new(json!({})), clock());
    let mut updates = policy.subscribe_changes();
    let started = host.first_read_started.notified();
    let reporter = policy.clone();
    let report_task = tokio::spawn(async move {
        reporter.report_client_activity(&session("session-1"), RpcClientId(1), report(json!({}))).await;
    });
    started.await;
    let remover = policy.clone();
    let remove_task = tokio::spawn(async move {
        remover.remove_rpc_client(&session("session-1"), RpcClientId(1)).await;
    });
    tokio::task::yield_now().await;
    gate.add_permits(1);
    report_task.await.unwrap();
    remove_task.await.unwrap();
    let _first = updates.next().await.unwrap();
    let last = updates.next().await.unwrap();
    assert_eq!(last.active_foreground_lease_count.get(), 0.0);
    assert!(last.active_scope_keys.is_empty());
}

#[tokio::test]
async fn bounds_client_id_churn_for_one_websocket_connection() {
    let policy = policy(power(json!({})), json!({}));
    for index in 0..=MAX_CLIENT_ACTIVITY_LEASES_PER_RPC_CLIENT {
        policy
            .report_client_activity(&session("session-1"), RpcClientId(1), report(json!({"clientId": format!("client-{index}")})))
            .await;
    }
    let snapshot = policy.snapshot().await;
    assert_eq!(snapshot.leases.len(), MAX_CLIENT_ACTIVITY_LEASES_PER_RPC_CLIENT);
    let newest = format!("client-{MAX_CLIENT_ACTIVITY_LEASES_PER_RPC_CLIENT}");
    assert!(snapshot.leases.iter().any(|lease| lease.client_id == newest));
}

#[tokio::test]
async fn host_conditions_gate_scoped_and_opportunistic_work() {
    // Low power mode: no opportunistic work, scoped demand stays visible but paused.
    let low_power = policy(power(json!({"lowPowerMode": "true", "stale": false})), json!({}));
    low_power.report_client_activity(&session("session-1"), RpcClientId(1), report(json!({}))).await;
    let snapshot = low_power.snapshot().await;
    assert_eq!(snapshot.active_foreground_lease_count.get(), 1.0);
    assert_eq!(snapshot.active_scope_keys, vec!["vcs-status:/repo".to_owned()]);
    assert!(!snapshot.should_run_opportunistic_work);
    assert!(low_power.has_demand(&scope("/repo")).await);
    assert!(!low_power.should_run_scope_work(&scope("/repo")).await);

    // Suspension.
    let suspended = policy(power(json!({"suspended": true, "stale": false})), json!({}));
    suspended.report_client_activity(&session("session-1"), RpcClientId(1), report(json!({}))).await;
    assert!(!suspended.snapshot().await.should_run_opportunistic_work);
    assert!(suspended.has_demand(&scope("/repo")).await);
    assert!(!suspended.should_run_scope_work(&scope("/repo")).await);

    // Stale host values never gate work.
    let stale = policy(
        power(json!({"locked": "true", "onBattery": "true", "lowPowerMode": "true", "thermalState": "critical", "stale": true})),
        json!({"backgroundActivityProfile": "battery-saver"}),
    );
    stale.report_client_activity(&session("session-1"), RpcClientId(1), report(json!({}))).await;
    assert!(stale.should_run_scope_work(&scope("/repo")).await);

    // Battery saver pauses on battery.
    let battery = policy(
        power(json!({"onBattery": "true", "stale": false})),
        json!({"backgroundActivityProfile": "battery-saver"}),
    );
    battery.report_client_activity(&session("session-1"), RpcClientId(1), report(json!({}))).await;
    assert!(!battery.should_run_scope_work(&scope("/repo")).await);
}

#[tokio::test]
async fn client_focus_and_profile_decide_scoped_work() {
    // Hidden client: demand visible, no scoped work.
    let hidden = policy(power(json!({})), json!({}));
    hidden
        .report_client_activity(&session("session-1"), RpcClientId(1), report(json!({"focused": false, "visible": false})))
        .await;
    let snapshot = hidden.snapshot().await;
    assert_eq!(snapshot.active_foreground_lease_count.get(), 0.0);
    assert_eq!(snapshot.active_scope_keys, vec!["vcs-status:/repo".to_owned()]);
    assert!(hidden.has_demand(&scope("/repo")).await);
    assert!(!hidden.should_run_scope_work(&scope("/repo")).await);

    // A recently used visible window that lost focus keeps working.
    let recent = policy(power(json!({})), json!({}));
    recent
        .report_client_activity(
            &session("session-1"),
            RpcClientId(1),
            report(json!({"focused": false, "recentlyInteracted": true})),
        )
        .await;
    assert_eq!(recent.snapshot().await.active_foreground_lease_count.get(), 1.0);
    assert!(recent.snapshot().await.should_run_opportunistic_work);
    assert!(recent.should_run_scope_work(&scope("/repo")).await);

    // Visible, unfocused, idle: paused.
    let idle = policy(power(json!({})), json!({}));
    idle.report_client_activity(
        &session("session-1"),
        RpcClientId(1),
        report(json!({"focused": false, "recentlyInteracted": false})),
    )
    .await;
    assert_eq!(idle.snapshot().await.active_foreground_lease_count.get(), 0.0);
    assert!(!idle.snapshot().await.should_run_opportunistic_work);
    assert!(!idle.should_run_scope_work(&scope("/repo")).await);

    // The performance profile allows background scoped work while a lease holds the scope.
    let performance = policy(power(json!({})), json!({"backgroundActivityProfile": "performance"}));
    performance
        .report_client_activity(&session("session-1"), RpcClientId(1), report(json!({"focused": false, "visible": false})))
        .await;
    assert!(performance.should_run_scope_work(&scope("/repo")).await);
}

#[tokio::test]
async fn subscribe_returns_the_snapshot_then_every_change_and_the_port_round_trips() {
    let policy = policy(power(json!({})), json!({}));
    let (latest, mut changes) = policy.subscribe().await;
    assert!(latest.leases.is_empty());
    policy.report_client_activity(&session("session-1"), RpcClientId(7), report(json!({}))).await;
    assert_eq!(changes.next().await.unwrap().leases.len(), 1);

    let port: &dyn zc_ports::BackgroundPolicy = &policy;
    let scope = zc_ports::contracts::BackgroundScope::VcsStatus { cwd: "/repo".into() };
    assert!(port.has_demand(&scope).await);
    assert!(port.should_run_scope_work(&scope).await);
    port.remove_rpc_client(&session("session-1"), &zc_ports::contracts::RpcClientId::new("7")).await;
    assert!(!port.has_demand(&scope).await);
    let snapshot = port.snapshot().await;
    assert_eq!(snapshot.0["activeScopeKeys"], json!([]));
}

// ---------------------------------------------------------------------------------------------
// HostPowerMonitor

fn electron(overrides: Value) -> HostPowerSnapshot {
    let mut value = json!({"source": "electron-main", "idle": "false", "idleSeconds": 0, "locked": "false", "suspended": false, "onBattery": "false",
                           "lowPowerMode": "unknown", "thermalState": "nominal", "stale": false, "updatedAt": "2026-06-17T12:00:00.000Z"});
    value.as_object_mut().unwrap().extend(overrides.as_object().unwrap().clone());
    decode(value)
}

fn epoch_unknown() -> HostPowerSnapshot {
    zc_checkpoints::background::unknown_snapshot(zc_contracts::HostPowerSource::Unknown, DateTimeUtc::from_millis(0).unwrap())
}

#[tokio::test]
async fn publishes_semantic_power_changes_without_idle_time_heartbeat_churn() {
    // The TS test runs on the TestClock (epoch): the unknown initial snapshot is that old.
    let monitor = HostPowerMonitor::new(Some(epoch_unknown()));
    monitor.report_now(electron(json!({})));
    let mut changes = monitor.subscribe_changes();
    monitor.report_now(electron(json!({"idleSeconds": 1, "updatedAt": "2026-06-17T12:00:01.000Z"})));
    monitor.report_now(electron(json!({"locked": "true", "updatedAt": "2026-06-17T12:00:02.000Z"})));
    assert_eq!(changes.next().await.unwrap().locked, BackgroundBooleanState::True);
}

#[tokio::test]
async fn ignores_host_power_reports_older_than_the_latest_accepted_snapshot() {
    let monitor = HostPowerMonitor::new(Some(electron(json!({"lowPowerMode": "false"}))));
    monitor.report_now(electron(
        json!({"lowPowerMode": "false", "locked": "true", "updatedAt": "2026-06-17T12:00:02.000Z"}),
    ));
    monitor.report_now(electron(json!({"lowPowerMode": "false", "updatedAt": "2026-06-17T12:00:01.000Z"})));
    let snapshot = monitor.current();
    assert_eq!(snapshot.locked, BackgroundBooleanState::True);
    assert_eq!(snapshot.updated_at, DateTimeUtc::parse("2026-06-17T12:00:02.000Z").unwrap());
}

#[tokio::test]
async fn consumes_desktop_power_directly() {
    let desktop: PubSub<HostPowerSnapshot> = PubSub::new();
    let (monitor, _task) = HostPowerMonitor::from_desktop(Some(epoch_unknown()), desktop.subscribe().boxed());
    let mut changes = monitor.subscribe_changes();
    desktop.publish(electron(json!({"onBattery": "true"})));
    assert_eq!(changes.next().await.unwrap().on_battery, BackgroundBooleanState::True);
}

#[tokio::test]
async fn subscribes_before_reading_the_desktop_snapshot_so_concurrent_power_updates_survive() {
    let desktop: PubSub<HostPowerSnapshot> = PubSub::new();
    let subscription = desktop.subscribe();
    desktop.publish(electron(json!({"onBattery": "true", "updatedAt": "2026-06-17T12:00:01.000Z"})));
    let (monitor, _task) = HostPowerMonitor::from_desktop(Some(electron(json!({}))), subscription.boxed());
    wait_for(|| monitor.current().on_battery == BackgroundBooleanState::True).await;
}
