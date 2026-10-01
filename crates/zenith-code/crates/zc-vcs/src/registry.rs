//! `vcs/VcsProjectConfig.ts`, `vcs/VcsDriverRegistry.ts` and `vcs/VcsProvisioningService.ts`.
//!
//! - The project config is `.t3code/vcs.json` in the cwd or any parent (`{"vcs":{"kind":…}}` or
//!   `{"vcsKind":…}`, lenient JSON). Any failure (inspect, read, decode) logs and means `auto`.
//! - The registry holds one driver per kind (only git today) and caches detection per
//!   `(requested kind, cwd)` for 2 s; negative and failed detections are not cached.
//! - Provisioning (`vcs.init`) defaults to git and refuses `unknown`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use crate::cache::OutcomeCache;
use crate::contracts::{RequestedVcsKind, VcsDriverKind, VcsInitInput, VcsRepositoryIdentity};
use crate::errors::{VcsError, VcsUnsupportedOperationError};
use crate::vcs_driver::VcsDriver;

const DETECTION_CACHE_CAPACITY: usize = 2_048;
const DETECTION_CACHE_TTL: Duration = Duration::from_secs(2);

/// `VcsProjectConfigError.operation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VcsProjectConfigOperation {
    Inspect,
    Read,
    Decode,
}

impl VcsProjectConfigOperation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Inspect => "inspect",
            Self::Read => "read",
            Self::Decode => "decode",
        }
    }
}

/// `VcsProjectConfigError` (logged, never returned).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcsProjectConfigError {
    pub operation: VcsProjectConfigOperation,
    pub cwd: String,
    pub config_path: String,
    pub cause: String,
}

impl VcsProjectConfigError {
    pub fn message(&self) -> String {
        format!("Failed to {} VCS project config at {}.", self.operation.as_str(), self.config_path)
    }
}

#[derive(Deserialize)]
struct ProjectVcsSection {
    #[serde(default)]
    kind: Option<VcsDriverKind>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectVcsConfig {
    #[serde(default)]
    vcs: Option<ProjectVcsSection>,
    #[serde(default)]
    vcs_kind: Option<VcsDriverKind>,
}

/// A sink for the config errors TS logs as warnings (tests capture them).
pub type ConfigErrorLogger = Arc<dyn Fn(&VcsProjectConfigError) + Send + Sync>;

/// `VcsProjectConfig`.
#[derive(Clone, Default)]
pub struct VcsProjectConfig {
    logger: Option<ConfigErrorLogger>,
}

impl VcsProjectConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_logger(logger: ConfigErrorLogger) -> Self {
        Self { logger: Some(logger) }
    }

    fn log(&self, error: VcsProjectConfigError) {
        tracing::warn!(
            operation = error.operation.as_str(),
            cwd = %error.cwd,
            config_path = %error.config_path,
            error_tag = "VcsProjectConfigError",
            "{}",
            error.message()
        );
        if let Some(logger) = &self.logger {
            logger(&error);
        }
    }

    async fn find_config_path(&self, cwd: &str) -> Option<PathBuf> {
        let mut current = PathBuf::from(cwd);
        loop {
            let candidate = current.join(".t3code").join("vcs.json");
            match tokio::fs::try_exists(&candidate).await {
                Ok(true) => return Some(candidate),
                Ok(false) => {}
                Err(io) => self.log(VcsProjectConfigError {
                    operation: VcsProjectConfigOperation::Inspect,
                    cwd: cwd.to_owned(),
                    config_path: candidate.to_string_lossy().into_owned(),
                    cause: io.to_string(),
                }),
            }
            let parent = match current.parent() {
                None => return None,
                Some(parent) if parent.as_os_str().is_empty() => PathBuf::from("."),
                Some(parent) => parent.to_path_buf(),
            };
            if parent == current {
                return None;
            }
            current = parent;
        }
    }

    async fn read_configured_kind(&self, cwd: &str, config_path: &Path) -> Result<RequestedVcsKind, VcsProjectConfigError> {
        let error = |operation, cause: String| VcsProjectConfigError {
            operation,
            cwd: cwd.to_owned(),
            config_path: config_path.to_string_lossy().into_owned(),
            cause,
        };
        let raw = tokio::fs::read_to_string(config_path)
            .await
            .map_err(|io| error(VcsProjectConfigOperation::Read, io.to_string()))?;
        let parsed: ProjectVcsConfig = zc_core::lenient_json::from_lenient_json(&raw).map_err(|e| error(VcsProjectConfigOperation::Decode, e.to_string()))?;
        Ok(parsed
            .vcs
            .and_then(|section| section.kind)
            .or(parsed.vcs_kind)
            .map(RequestedVcsKind::Kind)
            .unwrap_or(RequestedVcsKind::Auto))
    }

    /// `resolveKind({cwd, requestedKind?})`.
    pub async fn resolve_kind(&self, cwd: &str, requested: Option<RequestedVcsKind>) -> RequestedVcsKind {
        if let Some(RequestedVcsKind::Kind(kind)) = requested {
            return RequestedVcsKind::Kind(kind);
        }
        let Some(config_path) = self.find_config_path(cwd).await else {
            return RequestedVcsKind::Auto;
        };
        match self.read_configured_kind(cwd, &config_path).await {
            Ok(kind) => kind,
            Err(error) => {
                self.log(error);
                RequestedVcsKind::Auto
            }
        }
    }
}

/// `VcsDriverHandle`.
#[derive(Clone)]
pub struct VcsDriverHandle {
    pub kind: VcsDriverKind,
    pub repository: VcsRepositoryIdentity,
    pub driver: Arc<dyn VcsDriver>,
}

impl std::fmt::Debug for VcsDriverHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VcsDriverHandle")
            .field("kind", &self.kind)
            .field("repository", &self.repository)
            .finish_non_exhaustive()
    }
}

/// `VcsDriverRegistry`.
#[derive(Clone)]
pub struct VcsDriverRegistry {
    config: VcsProjectConfig,
    drivers: HashMap<VcsDriverKind, Arc<dyn VcsDriver>>,
    detection: OutcomeCache<String, Option<VcsDriverHandle>, VcsError>,
}

impl VcsDriverRegistry {
    /// A registry with the git driver.
    pub fn new(config: VcsProjectConfig, git: Arc<dyn VcsDriver>) -> Self {
        Self::with_drivers(config, [(VcsDriverKind::Git, git)])
    }

    /// A registry with several drivers (a future `jj` driver, test doubles). `auto` detection
    /// still only tries git, like TS.
    pub fn with_drivers(config: VcsProjectConfig, drivers: impl IntoIterator<Item = (VcsDriverKind, Arc<dyn VcsDriver>)>) -> Self {
        let drivers: HashMap<_, _> = drivers.into_iter().collect();
        Self {
            config,
            drivers,
            detection: OutcomeCache::new(DETECTION_CACHE_CAPACITY, |result, _| match result {
                Ok(Some(_)) => DETECTION_CACHE_TTL,
                _ => Duration::ZERO,
            }),
        }
    }

    /// `get(kind)`.
    pub fn get(&self, kind: VcsDriverKind) -> Result<Arc<dyn VcsDriver>, VcsError> {
        self.drivers.get(&kind).cloned().ok_or_else(|| {
            VcsError::UnsupportedOperation(VcsUnsupportedOperationError::new(
                "VcsDriverRegistry.get",
                kind,
                format!("No {kind} VCS driver is registered."),
            ))
        })
    }

    async fn detect_resolved_kind(&self, cwd: &str, requested: RequestedVcsKind) -> Result<Option<VcsDriverHandle>, VcsError> {
        let (kind, driver) = match requested {
            RequestedVcsKind::Kind(kind) if kind != VcsDriverKind::Unknown => (kind, self.get(kind)?),
            _ => (VcsDriverKind::Git, self.get(VcsDriverKind::Git)?),
        };
        Ok(driver
            .detect_repository(cwd)
            .await?
            .map(|repository| VcsDriverHandle { kind, repository, driver }))
    }

    /// `detect({cwd, requestedKind?})`.
    pub async fn detect(&self, cwd: &str, requested: Option<RequestedVcsKind>) -> Result<Option<VcsDriverHandle>, VcsError> {
        let requested = self.config.resolve_kind(cwd, requested).await;
        let key = format!("{}\0{cwd}", requested.as_str());
        let this = self.clone();
        let cwd = cwd.to_owned();
        self.detection
            .get(key, move || async move { this.detect_resolved_kind(&cwd, requested).await })
            .await
    }

    /// `resolve({cwd, requestedKind?})`: like `detect`, failing when nothing is detected.
    pub async fn resolve(&self, cwd: &str, requested: Option<RequestedVcsKind>) -> Result<VcsDriverHandle, VcsError> {
        if let Some(handle) = self.detect(cwd, requested).await? {
            return Ok(handle);
        }
        let requested = requested.unwrap_or(RequestedVcsKind::Auto);
        Err(VcsError::unsupported(
            "VcsDriverRegistry.resolve",
            requested,
            match requested {
                RequestedVcsKind::Auto => {
                    format!("No supported VCS repository was detected at {cwd}.")
                }
                RequestedVcsKind::Kind(kind) => {
                    format!("No {kind} repository was detected at {cwd}.")
                }
            },
        ))
    }
}

/// `VcsProvisioningService`.
#[derive(Clone)]
pub struct VcsProvisioningService {
    registry: VcsDriverRegistry,
}

impl VcsProvisioningService {
    pub fn new(registry: VcsDriverRegistry) -> Self {
        Self { registry }
    }

    /// `initRepository(input)`: git unless the input names another (concrete) kind.
    pub async fn init_repository(&self, input: &VcsInitInput) -> Result<(), VcsError> {
        let kind = match input.kind {
            None => VcsDriverKind::Git,
            Some(VcsDriverKind::Unknown) => {
                return Err(VcsError::UnsupportedOperation(VcsUnsupportedOperationError::new(
                    "VcsProvisioningService.resolveRequestedKind",
                    VcsDriverKind::Unknown,
                    "A concrete VCS driver kind is required for repository provisioning.",
                )))
            }
            Some(kind) => kind,
        };
        self.registry.get(kind)?.init_repository(input).await
    }
}
