//! `background/BackgroundPolicy.ts`: per-client activity leases plus the host power state
//! decide whether pollers may run (plan §6.19).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use zc_contracts::{
    AuthSessionId, BackgroundBooleanState, BackgroundPolicySnapshot, BackgroundScope, ClientActivityLease, ClientActivityLeaseAppState,
    ClientActivityLeaseBatteryState, ClientActivityReportInput, ClientActivityReportInputAppState, ClientActivityReportInputBatteryState, DateTimeUtc,
    HostPowerSnapshot, HostPowerThermalState, JsNumber, RpcClientId,
};
use zc_core::pubsub::PubSub;
use zc_ports::{EventStream, SettingsService};
use zc_settings::settings::background::{preset, resolve_server, ResolvedBackgroundActivity};

use super::host_power::HostPower;
use crate::reactor::ReactorTasks;

pub const DEFAULT_LEASE_TTL_MS: i64 = 45_000;
pub const MAX_LEASE_TTL_MS: i64 = 120_000;
pub const MAX_CLIENT_ACTIVITY_LEASES_PER_RPC_CLIENT: usize = 16;
/// Expired leases are swept (and a snapshot published) this often.
pub const LEASE_SWEEP_INTERVAL: Duration = Duration::from_secs(15);

/// `scopeKey(scope)`.
pub fn scope_key(scope: &BackgroundScope) -> String {
    match scope {
        BackgroundScope::ServerConfig(_) => "server-config".into(),
        BackgroundScope::Diagnostics(_) => "diagnostics".into(),
        BackgroundScope::ProviderStatus(scope) => match scope.instance_id.as_deref() {
            Some(instance_id) if !instance_id.is_empty() => format!("provider-status:{instance_id}"),
            _ => "provider-status".into(),
        },
        BackgroundScope::VcsStatus(scope) => format!("vcs-status:{}", scope.cwd),
        BackgroundScope::GitRefs(scope) => format!("git-refs:{}", scope.cwd),
        BackgroundScope::Thread(scope) => format!("thread:{}", scope.thread_id),
    }
}

fn lease_key(session_id: &AuthSessionId, rpc_client_id: RpcClientId, client_id: &str) -> String {
    serde_json::to_string(&(session_id.as_str(), rpc_client_id.0, client_id)).expect("strings and numbers encode")
}

fn is_lease_active(lease: &ClientActivityLease, now: DateTimeUtc) -> bool {
    lease.expires_at > now
}

fn is_foreground_lease(lease: &ClientActivityLease, now: DateTimeUtc) -> bool {
    is_lease_active(lease, now) && lease.visible && (lease.focused || lease.recently_interacted)
}

fn lease_has_scope(lease: &ClientActivityLease, scope: &BackgroundScope) -> bool {
    let key = scope_key(scope);
    lease.scopes.iter().any(|s| scope_key(s) == key)
}

fn has_thermal_pressure(host: &HostPowerSnapshot) -> bool {
    matches!(host.thermal_state, HostPowerThermalState::Serious | HostPowerThermalState::Critical)
}

fn is_host_constrained(host: &HostPowerSnapshot, settings: &ResolvedBackgroundActivity) -> bool {
    if host.stale {
        return false;
    }
    if host.suspended || (settings.pause_when_host_locked && host.locked == BackgroundBooleanState::True) || has_thermal_pressure(host) {
        return true;
    }
    if settings.pause_when_host_low_power && host.low_power_mode == BackgroundBooleanState::True {
        return true;
    }
    settings.pause_when_on_battery && host.on_battery == BackgroundBooleanState::True
}

fn is_client_constrained(lease: &ClientActivityLease, settings: &ResolvedBackgroundActivity) -> bool {
    if settings.pause_when_client_low_power && lease.low_power_mode == Some(BackgroundBooleanState::True) {
        return true;
    }
    settings.pause_when_on_battery && lease.battery_state == Some(ClientActivityLeaseBatteryState::Unplugged)
}

fn lease_may_run_scoped_work(lease: &ClientActivityLease, scope: &BackgroundScope, now: DateTimeUtc, settings: &ResolvedBackgroundActivity) -> bool {
    if !(is_lease_active(lease, now) && lease_has_scope(lease, scope)) || is_client_constrained(lease, settings) {
        return false;
    }
    if settings.profile == "performance" {
        return true;
    }
    is_foreground_lease(lease, now)
}

/// `[...keys].toSorted()`: UTF-16 code unit order.
fn sort_js(keys: &mut [String]) {
    keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
}

/// `computeSnapshot`.
fn compute_snapshot(
    host_power: HostPowerSnapshot,
    leases: &[(String, ClientActivityLease)],
    now: DateTimeUtc,
    settings: &ResolvedBackgroundActivity,
) -> BackgroundPolicySnapshot {
    let active: Vec<ClientActivityLease> = leases.iter().map(|(_, l)| l).filter(|l| is_lease_active(l, now)).cloned().collect();
    let foreground: Vec<&ClientActivityLease> = active.iter().filter(|l| is_foreground_lease(l, now)).collect();
    let mut keys: Vec<String> = Vec::new();
    for lease in &active {
        for scope in &lease.scopes {
            let key = scope_key(scope);
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    sort_js(&mut keys);
    let should_run_opportunistic_work = foreground.iter().any(|l| !is_client_constrained(l, settings)) && !is_host_constrained(&host_power, settings);
    BackgroundPolicySnapshot {
        host_power,
        active_foreground_lease_count: JsNumber::from(foreground.len() as f64),
        leases: active,
        active_scope_keys: keys,
        should_run_opportunistic_work,
        updated_at: now,
    }
}

/// `upsertClientActivityLease`: drops expired leases, then inserts or replaces; a connection
/// keeps at most [`MAX_CLIENT_ACTIVITY_LEASES_PER_RPC_CLIENT`] client ids (the oldest goes).
fn upsert_lease(leases: &mut Vec<(String, ClientActivityLease)>, key: String, lease: ClientActivityLease, now: DateTimeUtc) {
    leases.retain(|(_, current)| is_lease_active(current, now));
    if let Some(entry) = leases.iter_mut().find(|(k, _)| *k == key) {
        entry.1 = lease;
        return;
    }
    let mut count = 0;
    let mut oldest: Option<(usize, DateTimeUtc)> = None;
    for (index, (_, current)) in leases.iter().enumerate() {
        if current.session_id != lease.session_id || current.rpc_client_id != lease.rpc_client_id {
            continue;
        }
        count += 1;
        if oldest.is_none_or(|(_, at)| current.updated_at < at) {
            oldest = Some((index, current.updated_at));
        }
    }
    if count >= MAX_CLIENT_ACTIVITY_LEASES_PER_RPC_CLIENT {
        if let Some((index, _)) = oldest {
            leases.remove(index);
        }
    }
    leases.push((key, lease));
}

fn convert_app_state(state: ClientActivityReportInputAppState) -> ClientActivityLeaseAppState {
    match state {
        ClientActivityReportInputAppState::Active => ClientActivityLeaseAppState::Active,
        ClientActivityReportInputAppState::Inactive => ClientActivityLeaseAppState::Inactive,
        ClientActivityReportInputAppState::Background => ClientActivityLeaseAppState::Background,
        ClientActivityReportInputAppState::Unknown => ClientActivityLeaseAppState::Unknown,
    }
}

fn convert_battery(state: ClientActivityReportInputBatteryState) -> ClientActivityLeaseBatteryState {
    match state {
        ClientActivityReportInputBatteryState::Unknown => ClientActivityLeaseBatteryState::Unknown,
        ClientActivityReportInputBatteryState::Unplugged => ClientActivityLeaseBatteryState::Unplugged,
        ClientActivityReportInputBatteryState::Charging => ClientActivityLeaseBatteryState::Charging,
        ClientActivityReportInputBatteryState::Full => ClientActivityLeaseBatteryState::Full,
    }
}

/// The policy's clock (`DateTime.now`), replaceable in tests.
pub type Clock = Arc<dyn Fn() -> DateTimeUtc + Send + Sync>;

struct Inner {
    host: Arc<dyn HostPower>,
    settings: Arc<dyn SettingsService>,
    leases: Mutex<Vec<(String, ClientActivityLease)>>,
    changes: PubSub<BackgroundPolicySnapshot>,
    /// `publishMutex`: lease mutations and their publication happen one at a time.
    publish: tokio::sync::Mutex<()>,
    clock: Clock,
}

/// `BackgroundPolicy` (the service; it also implements [`zc_ports::BackgroundPolicy`]).
#[derive(Clone)]
pub struct BackgroundPolicyService {
    inner: Arc<Inner>,
}

impl BackgroundPolicyService {
    pub fn new(host: Arc<dyn HostPower>, settings: Arc<dyn SettingsService>) -> Self {
        Self::with_clock(host, settings, Arc::new(DateTimeUtc::now))
    }

    pub fn with_clock(host: Arc<dyn HostPower>, settings: Arc<dyn SettingsService>, clock: Clock) -> Self {
        Self {
            inner: Arc::new(Inner {
                host,
                settings,
                leases: Mutex::new(Vec::new()),
                changes: PubSub::new(),
                publish: tokio::sync::Mutex::new(()),
                clock,
            }),
        }
    }

    /// The forked watchers of `make`: republish on host power and settings changes, and sweep
    /// expired leases every 15 s. Dropping the result stops them.
    pub fn start(&self) -> ReactorTasks {
        let mut host_changes = self.inner.host.subscribe_changes();
        let mut settings_changes = self.inner.settings.subscribe_changes();
        let this = self.clone();
        let host_task = tokio::spawn(async move {
            while host_changes.next().await.is_some() {
                this.publish_snapshot().await;
            }
        });
        let this = self.clone();
        let settings_task = tokio::spawn(async move {
            while settings_changes.next().await.is_some() {
                this.publish_snapshot().await;
            }
        });
        let this = self.clone();
        let sweep_task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(LEASE_SWEEP_INTERVAL).await;
                let _guard = this.inner.publish.lock().await;
                let now = (this.inner.clock)();
                this.inner.leases.lock().unwrap().retain(|(_, lease)| is_lease_active(lease, now));
                this.publish_snapshot_unlocked().await;
            }
        });
        ReactorTasks::new(vec![host_task, settings_task, sweep_task])
    }

    async fn settings(&self) -> ResolvedBackgroundActivity {
        match self.inner.settings.get_settings().await {
            Ok(settings) => serde_json::to_value(&settings).map_or_else(|_| preset("balanced"), |value| resolve_server(&value)),
            Err(_) => preset("balanced"),
        }
    }

    /// `snapshot`.
    pub async fn snapshot(&self) -> BackgroundPolicySnapshot {
        let host_power = self.inner.host.snapshot().await;
        let leases = self.inner.leases.lock().unwrap().clone();
        let now = (self.inner.clock)();
        let settings = self.settings().await;
        compute_snapshot(host_power, &leases, now, &settings)
    }

    async fn publish_snapshot_unlocked(&self) {
        let snapshot = self.snapshot().await;
        self.inner.changes.publish(snapshot);
    }

    async fn publish_snapshot(&self) {
        let _guard = self.inner.publish.lock().await;
        self.publish_snapshot_unlocked().await;
    }

    /// `reportClientActivity(sessionId, rpcClientId, input)`.
    pub async fn report_client_activity(&self, session_id: &AuthSessionId, rpc_client_id: RpcClientId, input: ClientActivityReportInput) {
        let _guard = self.inner.publish.lock().await;
        let requested = input.ttl_ms.map_or(DEFAULT_LEASE_TTL_MS as f64, |ttl| ttl.get());
        let ttl_ms = requested.max(1_000.0).min(MAX_LEASE_TTL_MS as f64);
        let now = (self.inner.clock)();
        // `DateTime.add` with fractional milliseconds truncates toward the integer millisecond.
        let expires_at = DateTimeUtc::from_millis(now.as_millis() + ttl_ms as i64).unwrap_or(now);
        let lease = ClientActivityLease {
            session_id: session_id.clone(),
            rpc_client_id,
            client_id: input.client_id.clone(),
            client_kind: input.client_kind,
            visible: input.visible,
            focused: input.focused,
            recently_interacted: input.recently_interacted,
            app_state: input.app_state.map(convert_app_state),
            low_power_mode: input.low_power_mode,
            battery_state: input.battery_state.map(convert_battery),
            network_type: input.network_type.clone(),
            scopes: input.scopes.clone(),
            updated_at: now,
            expires_at,
        };
        let key = lease_key(session_id, rpc_client_id, &input.client_id);
        upsert_lease(&mut self.inner.leases.lock().unwrap(), key, lease, now);
        self.publish_snapshot_unlocked().await;
    }

    /// `removeRpcClient(sessionId, rpcClientId)`.
    pub async fn remove_rpc_client(&self, session_id: &AuthSessionId, rpc_client_id: RpcClientId) {
        let _guard = self.inner.publish.lock().await;
        self.inner
            .leases
            .lock()
            .unwrap()
            .retain(|(_, lease)| !(lease.session_id == *session_id && lease.rpc_client_id == rpc_client_id));
        self.publish_snapshot_unlocked().await;
    }

    /// `reportHostPowerState(snapshot)`: the host monitor's `report`.
    pub async fn report_host_power_state(&self, snapshot: HostPowerSnapshot) {
        self.inner.host.report(snapshot).await;
    }

    /// `streamChanges`: snapshots published from now on.
    pub fn subscribe_changes(&self) -> EventStream<BackgroundPolicySnapshot> {
        self.inner.changes.subscribe().boxed()
    }

    /// `subscribe`: the snapshot as of subscription plus every later one, under the publish
    /// mutex so nothing is lost or repeated in between.
    pub async fn subscribe(&self) -> (BackgroundPolicySnapshot, EventStream<BackgroundPolicySnapshot>) {
        let _guard = self.inner.publish.lock().await;
        let changes = self.inner.changes.subscribe();
        let latest = self.snapshot().await;
        (latest, changes.boxed())
    }

    /// `hasDemand(scope)`.
    pub async fn has_demand(&self, scope: &BackgroundScope) -> bool {
        let key = scope_key(scope);
        self.snapshot().await.active_scope_keys.contains(&key)
    }

    /// `shouldRunScopeWork(scope)`.
    pub async fn should_run_scope_work(&self, scope: &BackgroundScope) -> bool {
        let current = self.snapshot().await;
        let settings = self.settings().await;
        if is_host_constrained(&current.host_power, &settings) {
            return false;
        }
        current
            .leases
            .iter()
            .any(|lease| lease_may_run_scoped_work(lease, scope, current.updated_at, &settings))
    }

    /// `shouldRunOpportunisticWork`.
    pub async fn should_run_opportunistic_work(&self) -> bool {
        self.snapshot().await.should_run_opportunistic_work
    }
}

// ---------------------------------------------------------------------------------------------
// The port, over the wire-shaped placeholders of zc-ports.

fn port_scope(scope: &zc_ports::contracts::BackgroundScope) -> Option<BackgroundScope> {
    serde_json::to_value(scope).ok().and_then(|value| serde_json::from_value(value).ok())
}

/// The port's `RpcClientId` is a string; the contracts' is the socket's number.
fn port_rpc_client_id(id: &zc_ports::contracts::RpcClientId) -> RpcClientId {
    RpcClientId(id.as_str().parse().unwrap_or_default())
}

#[async_trait]
impl zc_ports::BackgroundPolicy for BackgroundPolicyService {
    async fn report_client_activity(
        &self,
        session_id: &AuthSessionId,
        rpc_client_id: &zc_ports::contracts::RpcClientId,
        input: zc_ports::contracts::ClientActivityReportInput,
    ) {
        match serde_json::from_value::<ClientActivityReportInput>(input.0) {
            Ok(input) => BackgroundPolicyService::report_client_activity(self, session_id, port_rpc_client_id(rpc_client_id), input).await,
            Err(error) => tracing::warn!(error = %error, "ignored an undecodable client activity report"),
        }
    }

    async fn remove_rpc_client(&self, session_id: &AuthSessionId, rpc_client_id: &zc_ports::contracts::RpcClientId) {
        BackgroundPolicyService::remove_rpc_client(self, session_id, port_rpc_client_id(rpc_client_id)).await;
    }

    async fn report_host_power_state(&self, snapshot: zc_ports::contracts::HostPowerSnapshot) {
        match serde_json::from_value::<HostPowerSnapshot>(snapshot.0) {
            Ok(snapshot) => BackgroundPolicyService::report_host_power_state(self, snapshot).await,
            Err(error) => tracing::warn!(error = %error, "ignored an undecodable host power report"),
        }
    }

    async fn snapshot(&self) -> zc_ports::contracts::BackgroundPolicySnapshot {
        let snapshot = BackgroundPolicyService::snapshot(self).await;
        zc_ports::contracts::BackgroundPolicySnapshot(serde_json::to_value(snapshot).unwrap_or_default())
    }

    async fn subscribe(&self) -> zc_ports::BackgroundPolicySubscription {
        let (latest, changes) = BackgroundPolicyService::subscribe(self).await;
        let encode = |snapshot: BackgroundPolicySnapshot| zc_ports::contracts::BackgroundPolicySnapshot(serde_json::to_value(snapshot).unwrap_or_default());
        zc_ports::BackgroundPolicySubscription {
            latest: encode(latest),
            changes: changes.map(encode).boxed(),
        }
    }

    async fn has_demand(&self, scope: &zc_ports::contracts::BackgroundScope) -> bool {
        match port_scope(scope) {
            Some(scope) => BackgroundPolicyService::has_demand(self, &scope).await,
            None => false,
        }
    }

    async fn should_run_scope_work(&self, scope: &zc_ports::contracts::BackgroundScope) -> bool {
        match port_scope(scope) {
            Some(scope) => BackgroundPolicyService::should_run_scope_work(self, &scope).await,
            None => false,
        }
    }

    async fn should_run_opportunistic_work(&self) -> bool {
        BackgroundPolicyService::should_run_opportunistic_work(self).await
    }
}
