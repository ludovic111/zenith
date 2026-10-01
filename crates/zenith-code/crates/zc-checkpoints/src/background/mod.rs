//! `background/**`: the host power monitor and the background policy built on it.

pub mod host_power;
pub mod policy;

pub use host_power::{unknown_snapshot, HostPower, HostPowerMonitor};
pub use policy::{scope_key, BackgroundPolicyService, MAX_CLIENT_ACTIVITY_LEASES_PER_RPC_CLIENT};
