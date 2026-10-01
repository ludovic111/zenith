//! `background/HostPowerMonitor.ts`: the latest host power snapshot, reported by the desktop
//! shell (`server.reportHostPowerState`) or the desktop telemetry receiver.

use std::sync::Mutex;

use async_trait::async_trait;
use futures::StreamExt;
use zc_contracts::{BackgroundBooleanState, DateTimeUtc, HostPowerSnapshot, HostPowerSource, HostPowerThermalState};
use zc_core::pubsub::PubSub;
use zc_ports::EventStream;

/// What the background policy reads from the host monitor (`HostPowerMonitor` service).
#[async_trait]
pub trait HostPower: Send + Sync {
    /// `snapshot`.
    async fn snapshot(&self) -> HostPowerSnapshot;
    /// `report(snapshot)`.
    async fn report(&self, snapshot: HostPowerSnapshot);
    /// `streamChanges`: semantic changes from now on.
    fn subscribe_changes(&self) -> EventStream<HostPowerSnapshot>;
}

/// `makeUnknownSnapshot(source, updatedAt)`: everything unknown, stale.
pub fn unknown_snapshot(source: HostPowerSource, updated_at: DateTimeUtc) -> HostPowerSnapshot {
    HostPowerSnapshot {
        source,
        idle: BackgroundBooleanState::Unknown,
        idle_seconds: None,
        locked: BackgroundBooleanState::Unknown,
        suspended: false,
        on_battery: BackgroundBooleanState::Unknown,
        low_power_mode: BackgroundBooleanState::Unknown,
        thermal_state: HostPowerThermalState::Unknown,
        stale: true,
        updated_at,
    }
}

/// `samePowerState`: equal except for idle seconds and the timestamp (heartbeats).
fn same_power_state(left: &HostPowerSnapshot, right: &HostPowerSnapshot) -> bool {
    left.source == right.source
        && left.idle == right.idle
        && left.locked == right.locked
        && left.suspended == right.suspended
        && left.on_battery == right.on_battery
        && left.low_power_mode == right.low_power_mode
        && left.thermal_state == right.thermal_state
        && left.stale == right.stale
}

/// `HostPowerMonitor`.
pub struct HostPowerMonitor {
    latest: Mutex<HostPowerSnapshot>,
    changes: PubSub<HostPowerSnapshot>,
}

impl HostPowerMonitor {
    /// `make(initialSnapshot?)`: the initial snapshot, or an unknown one as of now.
    pub fn new(initial: Option<HostPowerSnapshot>) -> Self {
        Self {
            latest: Mutex::new(initial.unwrap_or_else(|| unknown_snapshot(HostPowerSource::Unknown, DateTimeUtc::now()))),
            changes: PubSub::new(),
        }
    }

    /// `layer`: a monitor fed by the desktop telemetry receiver. `latest` and `changes` must
    /// come from one subscription taken before reading the latest value (the receiver's
    /// `subscribe`), so a power update racing the start is not lost. The returned task forwards
    /// `changes` (each desktop sample's `power`) into [`HostPowerMonitor::report_now`].
    pub fn from_desktop(latest: Option<HostPowerSnapshot>, mut changes: EventStream<HostPowerSnapshot>) -> (std::sync::Arc<Self>, tokio::task::JoinHandle<()>) {
        let monitor = std::sync::Arc::new(Self::new(latest));
        let feed = monitor.clone();
        let task = tokio::spawn(async move {
            while let Some(power) = changes.next().await {
                feed.report_now(power);
            }
        });
        (monitor, task)
    }

    /// The current snapshot.
    pub fn current(&self) -> HostPowerSnapshot {
        self.latest.lock().unwrap().clone()
    }

    /// `report(snapshot)`: older reports are ignored; the latest is always kept, but only a
    /// semantic change is published.
    pub fn report_now(&self, snapshot: HostPowerSnapshot) {
        let publish = {
            let mut latest = self.latest.lock().unwrap();
            if snapshot.updated_at < latest.updated_at {
                return;
            }
            let changed = !same_power_state(&latest, &snapshot);
            *latest = snapshot.clone();
            changed
        };
        if publish {
            self.changes.publish(snapshot);
        }
    }
}

#[async_trait]
impl HostPower for HostPowerMonitor {
    async fn snapshot(&self) -> HostPowerSnapshot {
        self.current()
    }

    async fn report(&self, snapshot: HostPowerSnapshot) {
        self.report_now(snapshot);
    }

    fn subscribe_changes(&self) -> EventStream<HostPowerSnapshot> {
        self.changes.subscribe().boxed()
    }
}
