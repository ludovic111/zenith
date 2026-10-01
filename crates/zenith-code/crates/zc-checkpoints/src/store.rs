//! `checkpointing/CheckpointStore.ts`: hidden-ref checkpoint capture, restore, diff and delete
//! for a workspace, through the active VCS driver's checkpoint capability (zc-vcs owns the git
//! commands; plan §5.5).

use async_trait::async_trait;
use zc_contracts::CheckpointRef;
use zc_vcs::contracts::{RequestedVcsKind, VcsDriverKind};
use zc_vcs::errors::VcsUnsupportedOperationError;
use zc_vcs::vcs_driver::DiffCheckpointsInput as VcsDiffInput;
use zc_vcs::{VcsDriverRegistry, VcsError};

use crate::errors::CheckpointStoreError;

/// `DiffCheckpointsInput`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffCheckpointsInput {
    pub cwd: String,
    pub from_checkpoint_ref: CheckpointRef,
    pub to_checkpoint_ref: CheckpointRef,
    pub fallback_from_to_head: bool,
    pub ignore_whitespace: bool,
    pub format: DiffFormat,
}

/// `format?: "patch" | "numstat"` (patch by default).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffFormat {
    #[default]
    Patch,
    /// NUL-delimited `--numstat -z` (see [`crate::diffs`]).
    Numstat,
}

/// `CheckpointStore`. A trait so the reactor and diff query tests can stand in for git.
#[async_trait]
pub trait CheckpointStore: Send + Sync {
    /// `isGitRepository(cwd)`.
    async fn is_git_repository(&self, cwd: &str) -> Result<bool, CheckpointStoreError>;
    /// `captureCheckpoint({cwd, checkpointRef})`: an isolated temporary index, a hidden ref.
    async fn capture_checkpoint(&self, cwd: &str, checkpoint_ref: &CheckpointRef) -> Result<(), CheckpointStoreError>;
    /// `hasCheckpointRef({cwd, checkpointRef})`.
    async fn has_checkpoint_ref(&self, cwd: &str, checkpoint_ref: &CheckpointRef) -> Result<bool, CheckpointStoreError>;
    /// `restoreCheckpoint({cwd, checkpointRef, fallbackToHead?})`: false when there is nothing
    /// to restore.
    async fn restore_checkpoint(&self, cwd: &str, checkpoint_ref: &CheckpointRef, fallback_to_head: bool) -> Result<bool, CheckpointStoreError>;
    /// `diffCheckpoints(input)`.
    async fn diff_checkpoints(&self, input: &DiffCheckpointsInput) -> Result<String, CheckpointStoreError>;
    /// `deleteCheckpointRefs({cwd, checkpointRefs})`: missing refs are tolerated.
    async fn delete_checkpoint_refs(&self, cwd: &str, checkpoint_refs: &[CheckpointRef]) -> Result<(), CheckpointStoreError>;
}

/// The live store over the VCS driver registry (`CheckpointStore.layer`).
#[derive(Clone)]
pub struct VcsCheckpointStore {
    registry: VcsDriverRegistry,
}

impl VcsCheckpointStore {
    pub fn new(registry: VcsDriverRegistry) -> Self {
        Self { registry }
    }

    /// `resolveCheckpoints(operation, cwd)`: the driver's checkpoint capability, or
    /// `VcsUnsupportedOperationError`.
    async fn resolve(&self, operation: &str, cwd: &str) -> Result<zc_vcs::registry::VcsDriverHandle, VcsError> {
        let handle = self.registry.resolve(cwd, None).await?;
        if handle.driver.checkpoints().is_none() {
            return Err(VcsError::UnsupportedOperation(VcsUnsupportedOperationError::new(
                operation,
                handle.kind,
                format!("{} driver does not implement checkpoint operations.", handle.kind),
            )));
        }
        Ok(handle)
    }
}

macro_rules! with_checkpoints {
    ($self:ident, $operation:literal, $cwd:expr, |$ops:ident| $body:expr) => {{
        let handle = $self.resolve($operation, $cwd).await?;
        let $ops = handle.driver.checkpoints().expect("checked by resolve");
        $body
    }};
}

#[async_trait]
impl CheckpointStore for VcsCheckpointStore {
    async fn is_git_repository(&self, cwd: &str) -> Result<bool, CheckpointStoreError> {
        Ok(self.registry.detect(cwd, Some(RequestedVcsKind::Kind(VcsDriverKind::Git))).await?.is_some())
    }

    async fn capture_checkpoint(&self, cwd: &str, checkpoint_ref: &CheckpointRef) -> Result<(), CheckpointStoreError> {
        with_checkpoints!(self, "CheckpointStore.captureCheckpoint", cwd, |ops| ops
            .capture_checkpoint(cwd, checkpoint_ref.as_str())
            .await)
    }

    async fn has_checkpoint_ref(&self, cwd: &str, checkpoint_ref: &CheckpointRef) -> Result<bool, CheckpointStoreError> {
        with_checkpoints!(self, "CheckpointStore.hasCheckpointRef", cwd, |ops| ops
            .has_checkpoint_ref(cwd, checkpoint_ref.as_str())
            .await)
    }

    async fn restore_checkpoint(&self, cwd: &str, checkpoint_ref: &CheckpointRef, fallback_to_head: bool) -> Result<bool, CheckpointStoreError> {
        with_checkpoints!(self, "CheckpointStore.restoreCheckpoint", cwd, |ops| ops
            .restore_checkpoint(cwd, checkpoint_ref.as_str(), fallback_to_head)
            .await)
    }

    async fn diff_checkpoints(&self, input: &DiffCheckpointsInput) -> Result<String, CheckpointStoreError> {
        let vcs_input = VcsDiffInput {
            cwd: input.cwd.clone(),
            from_checkpoint_ref: input.from_checkpoint_ref.as_str().to_owned(),
            to_checkpoint_ref: input.to_checkpoint_ref.as_str().to_owned(),
            fallback_from_to_head: input.fallback_from_to_head,
            ignore_whitespace: input.ignore_whitespace,
            numstat: input.format == DiffFormat::Numstat,
        };
        with_checkpoints!(self, "CheckpointStore.diffCheckpoints", &input.cwd, |ops| ops
            .diff_checkpoints(&vcs_input)
            .await)
    }

    async fn delete_checkpoint_refs(&self, cwd: &str, checkpoint_refs: &[CheckpointRef]) -> Result<(), CheckpointStoreError> {
        let refs: Vec<String> = checkpoint_refs.iter().map(|r| r.as_str().to_owned()).collect();
        with_checkpoints!(self, "CheckpointStore.deleteCheckpointRefs", cwd, |ops| ops
            .delete_checkpoint_refs(cwd, &refs)
            .await)
    }
}
