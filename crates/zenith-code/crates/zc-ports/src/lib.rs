//! zc-ports: the service traits that let zenith code's subsystems depend on each other without
//! depending on each other's crates (plan §7.3).
//!
//! In TS the Effect layers form cycles: orchestration reactors call providers, git and text
//! generation; providers and git dispatch orchestration commands; text generation needs the
//! provider instances; pollers ask the background policy, which reads settings. In Rust those
//! edges go through the traits below, defined once here and implemented by the owning crate.
//! The server crate (`zenith-code`) builds the concrete services and hands each consumer
//! `Arc<dyn Trait>`s, the way `server.ts` composes layers.
//!
//! | Trait | TS service | Implemented by |
//! |---|---|---|
//! | [`ProviderService`] | `provider/Services/ProviderService.ts` | zc-providers |
//! | [`ProviderStatusReads`], [`ProviderAuthCommands`] | `ProviderRegistry.getProviders`, `ProviderAuthService.tryHandlePromptCommand` | zc-providers |
//! | [`OrchestrationDispatch`] | `orchestration/Services/OrchestrationEngine.ts` | zc-orchestration |
//! | [`ProjectionReads`] | `orchestration/Services/ProjectionSnapshotQuery.ts` | zc-orchestration |
//! | [`TerminalManager`] | `terminal/Manager.ts` | zc-terminal |
//! | [`GitWorkflow`], [`VcsStatusRefresher`] | `git/GitWorkflowService.ts` (+ `GitManager.branchPullRequest`), `vcs/VcsStatusBroadcaster.ts` refreshes | zc-vcs |
//! | [`TextGeneration`] | `textGeneration/TextGeneration.ts` | zc-textgen |
//! | [`PullRequests`] | `pullRequest/PullRequestService.ts` (reactor slice) | zc-pullrequest |
//! | [`SettingsService`] | `serverSettings.ts` `ServerSettingsService` | zc-settings |
//! | [`BackgroundPolicy`] | `background/BackgroundPolicy.ts` | zc-orchestration |
//!
//! Conventions:
//! - Traits are `Send + Sync` and dyn-compatible (`#[async_trait]`), used as `Arc<dyn Trait>`.
//! - Effect `Stream`s become [`EventStream`]s. Every `subscribe_*` is **eager**: the
//!   subscription exists when the method returns, so a caller can subscribe and then read a
//!   snapshot without losing what happens in between. Streams are unbounded (no lagging, no
//!   drops). Dropping the stream unsubscribes.
//! - Wire types are placeholders in [`contracts`] until `zc-contracts` lands; each names the
//!   exact contract type it becomes.

pub mod adapter;
pub mod background;
pub mod contracts;
pub mod git;
pub mod orchestration;
pub mod provider;
pub mod pull_requests;
pub mod settings;
pub mod terminal;
pub mod text_generation;

use std::sync::Arc;

pub use background::{BackgroundPolicy, BackgroundPolicySubscription};
pub use contracts::TaggedError;
pub use git::{GitWorkflow, VcsStatusRefresher};
pub use orchestration::{DispatchResult, OrchestrationDispatch, ProjectionReads};
pub use provider::{ProviderAuthCommands, ProviderService, ProviderStatusReads};
pub use pull_requests::PullRequests;
pub use settings::SettingsService;
pub use terminal::TerminalManager;
pub use text_generation::TextGeneration;

/// A boxed, `Send`, `'static` stream: the Rust shape of an Effect `Stream` handed across ports.
pub type EventStream<T> = futures::stream::BoxStream<'static, T>;

/// Every port, as the server wiring hands them out. Crates take the subset they need.
#[derive(Clone)]
pub struct Ports {
    pub provider_service: Arc<dyn ProviderService>,
    pub provider_status: Arc<dyn ProviderStatusReads>,
    pub provider_auth: Arc<dyn ProviderAuthCommands>,
    pub orchestration: Arc<dyn OrchestrationDispatch>,
    pub projections: Arc<dyn ProjectionReads>,
    pub terminals: Arc<dyn TerminalManager>,
    pub git: Arc<dyn GitWorkflow>,
    pub vcs_status: Arc<dyn VcsStatusRefresher>,
    pub text_generation: Arc<dyn TextGeneration>,
    pub pull_requests: Arc<dyn PullRequests>,
    pub settings: Arc<dyn SettingsService>,
    pub background_policy: Arc<dyn BackgroundPolicy>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{ServerSettings, ServerSettingsError, ServerSettingsPatch};
    use async_trait::async_trait;
    use futures::StreamExt;
    use serde_json::json;
    use std::sync::Mutex;

    /// Every port must stay usable as `Arc<dyn Trait>`.
    #[allow(dead_code)]
    fn dyn_compatible(ports: &Ports) -> usize {
        let _: &dyn ProviderService = &*ports.provider_service;
        let _: &dyn ProviderStatusReads = &*ports.provider_status;
        let _: &dyn ProviderAuthCommands = &*ports.provider_auth;
        let _: &dyn OrchestrationDispatch = &*ports.orchestration;
        let _: &dyn ProjectionReads = &*ports.projections;
        let _: &dyn TerminalManager = &*ports.terminals;
        let _: &dyn GitWorkflow = &*ports.git;
        let _: &dyn VcsStatusRefresher = &*ports.vcs_status;
        let _: &dyn TextGeneration = &*ports.text_generation;
        let _: &dyn PullRequests = &*ports.pull_requests;
        let _: &dyn SettingsService = &*ports.settings;
        let _: &dyn BackgroundPolicy = &*ports.background_policy;
        12
    }

    /// A minimal in-memory implementation, showing the eager-subscription contract.
    struct MemorySettings {
        current: Mutex<ServerSettings>,
        subscribers: Mutex<Vec<futures::channel::mpsc::UnboundedSender<ServerSettings>>>,
    }

    #[async_trait]
    impl SettingsService for MemorySettings {
        async fn get_settings(&self) -> Result<ServerSettings, ServerSettingsError> {
            Ok(self.current.lock().unwrap().clone())
        }

        async fn update_settings(&self, patch: ServerSettingsPatch) -> Result<ServerSettings, ServerSettingsError> {
            let mut next = self.current.lock().unwrap().clone();
            if let Some(mode) = patch.response_streaming_mode {
                next.response_streaming_mode = mode;
            }
            *self.current.lock().unwrap() = next.clone();
            for subscriber in self.subscribers.lock().unwrap().iter() {
                let _ = subscriber.unbounded_send(next.clone());
            }
            Ok(next)
        }

        fn subscribe_changes(&self) -> EventStream<ServerSettings> {
            let (sender, receiver) = futures::channel::mpsc::unbounded();
            self.subscribers.lock().unwrap().push(sender);
            receiver.boxed()
        }
    }

    #[tokio::test]
    async fn ports_work_behind_arc_dyn() {
        let settings: Arc<dyn SettingsService> = Arc::new(MemorySettings {
            current: Mutex::new(serde_json::from_value(json!({})).unwrap()),
            subscribers: Mutex::new(Vec::new()),
        });
        let mut changes = settings.subscribe_changes();
        settings
            .update_settings(serde_json::from_value(json!({"responseStreamingMode": "turn"})).unwrap())
            .await
            .unwrap();
        let changed = changes.next().await.unwrap();
        assert_eq!(serde_json::to_value(&changed).unwrap()["responseStreamingMode"], json!("turn"));
        let current = settings.get_settings().await.unwrap();
        assert_eq!(serde_json::to_value(&current).unwrap()["responseStreamingMode"], json!("turn"));
    }
}
