//! The desktop (Electron) side of resource telemetry, `DesktopTelemetryReceiver.ts`. zenith has
//! no Electron shell: the server never gets a telemetry descriptor, so the source is always
//! [`NoDesktopTelemetry`] ("unavailable", like the TS server outside the desktop app). The
//! trait stays so the merge logic keeps its Electron paths and their tests.

use zc_contracts::{DateTimeUtc, DesktopHostTelemetrySnapshot, ResourceTelemetrySourceStatus};

/// `DesktopTelemetryReceiverHealth`.
#[derive(Debug, Clone, PartialEq)]
pub struct DesktopHealth {
    pub status: ResourceTelemetrySourceStatus,
    pub last_sample_at: Option<DateTimeUtc>,
    pub last_error: Option<String>,
}

pub trait DesktopTelemetry: Send + Sync + 'static {
    fn latest(&self) -> Option<DesktopHostTelemetrySnapshot>;
    fn health(&self) -> DesktopHealth;
    /// Live diagnostics are on screen (the desktop app samples Electron faster).
    fn set_diagnostics_demand(&self, enabled: bool);
}

/// No desktop telemetry descriptor (`DesktopTelemetryDescriptorUnavailable`).
#[derive(Debug, Clone)]
pub struct NoDesktopTelemetry {
    /// The runtime mode, `web` or `desktop`.
    pub mode: String,
}

impl DesktopTelemetry for NoDesktopTelemetry {
    fn latest(&self) -> Option<DesktopHostTelemetrySnapshot> {
        None
    }

    fn health(&self) -> DesktopHealth {
        DesktopHealth {
            status: ResourceTelemetrySourceStatus::Unavailable,
            last_sample_at: None,
            last_error: Some(format!("Desktop telemetry descriptor is unavailable in '{}' mode.", self.mode)),
        }
    }

    fn set_diagnostics_demand(&self, _enabled: bool) {}
}
