//! zenith code's assets (`apps/server/src/assets/**`, plan §6.11, WP-29): signed capability URLs
//! for the files the web app shows, attachment uploads, byte ranges for audio and video, GitHub
//! media through the `gh` credential, macOS app icons, and the project favicon resolver
//! (`project/ProjectFaviconResolver.ts`, `project/T3ProjectFileLoader.ts`).
//!
//! | Piece | Module |
//! |---|---|
//! | `AssetAccess` (`assets.createUrl`, the claims, `resolveAsset`) | [`access`] |
//! | `AttachmentUpload` (`attachments.createUploadUrl`, `attachments.delete`, the store) | [`upload`] |
//! | `GET\|HEAD /api/assets/*`, `POST /api/attachments/upload/*` | [`http`] |
//! | the three RPC methods | [`rpc`] |
//! | `MediaFile` | [`media_file`] |
//! | `GitHubMediaFetch`, `@t3tools/shared/githubMedia` | [`github_media`] |
//! | `NativeAppIconResolver` | [`native_app_icon`] |
//! | `ProjectFaviconResolver` | [`favicon`] |
//! | `T3ProjectFileLoader` | [`project_file`] |
//! | `@t3tools/shared/filePreview`, `imageDimensions` | [`preview`], [`image_dimensions`] |
//!
//! `imageMime.ts` and the attachment store already live in `zc_orchestration::attachments`;
//! this crate uses them. Signed URLs use the TS key file (`secrets/asset-access-signing-key.bin`)
//! and the TS token format, so URLs minted by either server verify in the other.

// The errors are the wire types of the contracts, which carry the requested resource.
#![allow(clippy::result_large_err)]

pub mod access;
pub mod favicon;
pub mod github_media;
pub mod http;
pub mod image_dimensions;
pub mod media_file;
pub mod native_app_icon;
pub mod preview;
pub mod project_file;
pub mod rpc;
pub mod upload;

use std::path::Path;
use std::sync::Arc;

use zc_auth::SharedClock;
use zc_core::ServerSecretStore;
use zc_ports::ProjectionReads;
use zc_rpc::RpcRouterBuilder;

pub use access::{AssetAccess, AssetClaims, IssueAssetUrlInput, ResolvedAsset, ResolvedFile, ASSET_ROUTE_PREFIX};
pub use favicon::ProjectFaviconResolver;
pub use github_media::GitHubMediaFetch;
pub use native_app_icon::NativeAppIconResolver;
pub use upload::{AttachmentUploads, ATTACHMENT_UPLOAD_ROUTE_PREFIX};

/// The boolean descriptor capabilities this crate implements (plus `fileAttachments`, see
/// [`file_attachments_capability`]).
pub const CAPABILITIES: &[&str] = &["attachmentUploads", "questionAttachments"];

/// `fileAttachments: { maxUploadBytes: PROVIDER_SEND_TURN_MAX_FILE_BYTES }`.
pub fn file_attachments_capability() -> serde_json::Value {
    serde_json::json!({ "maxUploadBytes": rpc::MAX_FILE_UPLOAD_BYTES })
}

/// Everything the routes and RPC methods share.
pub struct Assets {
    pub access: AssetAccess,
    pub uploads: AttachmentUploads,
    pub github: GitHubMediaFetch,
}

impl std::fmt::Debug for Assets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Assets").field("access", &self.access).finish_non_exhaustive()
    }
}

impl Assets {
    /// The production services: `attachments_dir` (`<stateDir>/attachments`), the provider
    /// status cache directory (`<baseDir>/caches`, where app icons are cached), the secret
    /// store, the system clock.
    pub fn new(attachments_dir: &Path, provider_status_cache_dir: &Path, secrets: ServerSecretStore) -> Self {
        Self::with_parts(
            attachments_dir,
            secrets,
            zc_auth::system_clock(),
            ProjectFaviconResolver::default(),
            NativeAppIconResolver::new(provider_status_cache_dir),
            GitHubMediaFetch::new(
                Arc::new(github_media::ReqwestMediaHttp::default()),
                Arc::new(github_media::GhCliTokenSource),
                zc_auth::system_clock(),
            ),
        )
    }

    /// Every part injectable (tests).
    pub fn with_parts(
        attachments_dir: &Path,
        secrets: ServerSecretStore,
        clock: SharedClock,
        favicons: ProjectFaviconResolver,
        native_icons: NativeAppIconResolver,
        github: GitHubMediaFetch,
    ) -> Self {
        Self {
            access: AssetAccess::new(attachments_dir, secrets.clone(), clock.clone(), favicons, native_icons),
            uploads: AttachmentUploads::new(attachments_dir, secrets, clock),
            github,
        }
    }

    /// `GET|HEAD /api/assets/*` and `POST /api/attachments/upload/*`.
    pub fn routes(self: &Arc<Self>) -> axum::Router {
        http::routes(self.clone())
    }

    /// `assets.createUrl`, `attachments.createUploadUrl`, `attachments.delete`.
    pub fn register_rpc(self: &Arc<Self>, builder: RpcRouterBuilder, reads: Arc<dyn ProjectionReads>) -> RpcRouterBuilder {
        rpc::register(builder, self.clone(), reads)
    }

    /// The startup sweep of stale pending and partial uploads (`ensureServerDirectories`).
    pub fn sweep_at_startup(&self) -> usize {
        zc_orchestration::attachments::sweep_at_startup(Path::new(&self.access.attachments_dir))
    }
}
