//! Port of `Services/ProviderAdapterRegistry.ts` / `Layers/ProviderAdapterRegistry.ts`: the
//! lookup boundary the provider service routes through. [`InstanceAdapterRegistry`] reads the
//! live [`ProviderInstanceRegistry`] on every call, so settings-driven hot reload shows up
//! immediately; tests implement [`AdapterRegistry`] over a fixed set of adapters.

use std::sync::Arc;

use futures::StreamExt;
use zc_contracts::{ProviderDriverKind, ProviderInstanceId};
use zc_ports::adapter::ProviderAdapter;
use zc_ports::EventStream;

use crate::driver::ContinuationIdentity;
use crate::errors::ProviderServiceError;
use crate::instance_registry::ProviderInstanceRegistry;

/// `ProviderInstanceRoutingInfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingInfo {
    pub instance_id: ProviderInstanceId,
    pub driver_kind: ProviderDriverKind,
    pub display_name: Option<String>,
    pub accent_color: Option<String>,
    pub enabled: bool,
    pub continuation_identity: ContinuationIdentity,
}

/// `ProviderAdapterRegistryShape`.
pub trait AdapterRegistry: Send + Sync {
    /// `getByInstance`: `ProviderUnsupportedError` (named after the instance id) when no live
    /// instance has this id, including configured-but-unavailable ones.
    fn get_by_instance(&self, instance_id: &ProviderInstanceId) -> Result<Arc<dyn ProviderAdapter>, ProviderServiceError>;
    /// `getInstanceInfo`.
    fn get_instance_info(&self, instance_id: &ProviderInstanceId) -> Result<RoutingInfo, ProviderServiceError>;
    /// `listInstances`: live instance ids only.
    fn list_instances(&self) -> Vec<ProviderInstanceId>;
    /// `subscribeChanges`: ticks when instances are added, removed or rebuilt (eager).
    fn subscribe_changes(&self) -> EventStream<()>;
}

fn unsupported(instance_id: &ProviderInstanceId) -> ProviderServiceError {
    ProviderServiceError::Unsupported {
        provider: instance_id.to_string(),
    }
}

/// `ProviderAdapterRegistryLive`: a facade over the instance registry.
#[derive(Clone)]
pub struct InstanceAdapterRegistry {
    registry: ProviderInstanceRegistry,
}

impl InstanceAdapterRegistry {
    pub fn new(registry: ProviderInstanceRegistry) -> Self {
        Self { registry }
    }
}

impl AdapterRegistry for InstanceAdapterRegistry {
    fn get_by_instance(&self, instance_id: &ProviderInstanceId) -> Result<Arc<dyn ProviderAdapter>, ProviderServiceError> {
        self.registry.get_routing_adapter(instance_id).ok_or_else(|| unsupported(instance_id))
    }

    fn get_instance_info(&self, instance_id: &ProviderInstanceId) -> Result<RoutingInfo, ProviderServiceError> {
        let instance = self.registry.get_instance(instance_id).ok_or_else(|| unsupported(instance_id))?;
        Ok(RoutingInfo {
            instance_id: instance.instance_id.clone(),
            driver_kind: instance.driver_kind.clone(),
            display_name: instance.display_name.clone(),
            accent_color: instance.accent_color.clone(),
            enabled: instance.enabled,
            continuation_identity: instance.continuation_identity.clone(),
        })
    }

    fn list_instances(&self) -> Vec<ProviderInstanceId> {
        self.registry.list_instances().iter().map(|instance| instance.instance_id.clone()).collect()
    }

    fn subscribe_changes(&self) -> EventStream<()> {
        self.registry.subscribe_changes().boxed()
    }
}

/// A fixed `instance id → adapter` map (`makeStaticInstanceRegistry` of the TS tests, also handy
/// for embedding a single adapter).
pub struct StaticAdapterRegistry {
    entries: Vec<(ProviderInstanceId, Arc<dyn ProviderAdapter>, bool)>,
    changes: zc_core::PubSub<()>,
}

impl StaticAdapterRegistry {
    /// Every instance enabled.
    pub fn new(entries: Vec<(ProviderInstanceId, Arc<dyn ProviderAdapter>)>) -> Self {
        Self {
            entries: entries.into_iter().map(|(id, adapter)| (id, adapter, true)).collect(),
            changes: zc_core::PubSub::new(),
        }
    }

    /// With explicit enabled flags.
    pub fn with_enabled(entries: Vec<(ProviderInstanceId, Arc<dyn ProviderAdapter>, bool)>) -> Self {
        Self {
            entries,
            changes: zc_core::PubSub::new(),
        }
    }
}

impl AdapterRegistry for StaticAdapterRegistry {
    fn get_by_instance(&self, instance_id: &ProviderInstanceId) -> Result<Arc<dyn ProviderAdapter>, ProviderServiceError> {
        self.entries
            .iter()
            .find(|(id, _, _)| id == instance_id)
            .map(|(_, adapter, _)| adapter.clone())
            .ok_or_else(|| unsupported(instance_id))
    }

    fn get_instance_info(&self, instance_id: &ProviderInstanceId) -> Result<RoutingInfo, ProviderServiceError> {
        let (id, adapter, enabled) = self
            .entries
            .iter()
            .find(|(id, _, _)| id == instance_id)
            .ok_or_else(|| unsupported(instance_id))?;
        let driver_kind = adapter.provider();
        Ok(RoutingInfo {
            instance_id: id.clone(),
            continuation_identity: ContinuationIdentity::default_for(&driver_kind, id),
            driver_kind,
            display_name: None,
            accent_color: None,
            enabled: *enabled,
        })
    }

    fn list_instances(&self) -> Vec<ProviderInstanceId> {
        self.entries.iter().map(|(id, _, _)| id.clone()).collect()
    }

    fn subscribe_changes(&self) -> EventStream<()> {
        self.changes.subscribe().boxed()
    }
}
