//! The asset RPC methods of `ws.ts`:
//!
//! | Method | Scope | Behaviour |
//! |---|---|---|
//! | `assets.createUrl` | `orchestration:read` | resolves the workspace (thread worktree or project root; a project for a favicon), then [`AssetAccess::issue_asset_url`] |
//! | `attachments.createUploadUrl` | `orchestration:operate` | [`AttachmentUploads::issue_upload_url`] |
//! | `attachments.delete` | `orchestration:operate` | [`AttachmentUploads::delete_pending_attachment`] |
//!
//! Payloads are checked like the TS schemas decode them (trimmed strings, non-empty, maximum
//! lengths, size limits); a payload that fails is a per-request defect, like a decode failure.

use std::sync::Arc;

use regex::Regex;
use zc_contracts::{
    AssetAccessError, AssetCreateUrlInput, AssetCreateUrlResult, AssetResource, AssetWorkspaceContextResolutionError, AttachmentCreateUploadUrlInput,
    AttachmentCreateUploadUrlResult, AttachmentDeleteInput, AttachmentUploadSigningKeyError, LitAssetWorkspaceContextResolutionError,
    ToolActivityNativeAppReference,
};
use zc_core::defect::js_length;
use zc_core::Defect;
use zc_ports::ProjectionReads;
use zc_rpc::{Failure, RpcMethod, RpcRouterBuilder};

use crate::access::{errors, IssueAssetUrlInput};
use crate::preview::is_absolute;
use crate::Assets;

/// `PROVIDER_SEND_TURN_MAX_IMAGE_BYTES`.
pub const MAX_IMAGE_UPLOAD_BYTES: i64 = 10 * 1024 * 1024;
/// `PROVIDER_SEND_TURN_MAX_FILE_BYTES` (also the `fileAttachments.maxUploadBytes` capability).
pub const MAX_FILE_UPLOAD_BYTES: i64 = 50 * 1024 * 1024;
const ASSET_PATH_MAX_LENGTH: usize = 1024;

macro_rules! method {
    ($name:ident, $tag:literal, $payload:ty, $success:ty, $error:ty) => {
        #[doc = concat!("`", $tag, "`.")]
        pub struct $name;
        impl RpcMethod for $name {
            const TAG: &'static str = $tag;
            const STREAM: bool = false;
            type Payload = $payload;
            type Success = $success;
            type Error = $error;
        }
    };
}

method!(AssetsCreateUrl, "assets.createUrl", AssetCreateUrlInput, AssetCreateUrlResult, AssetAccessError);
method!(
    AttachmentsCreateUploadUrl,
    "attachments.createUploadUrl",
    AttachmentCreateUploadUrlInput,
    AttachmentCreateUploadUrlResult,
    AttachmentUploadSigningKeyError
);
method!(
    AttachmentsDelete,
    "attachments.delete",
    AttachmentDeleteInput,
    (),
    AttachmentUploadSigningKeyError
);

// ---------------------------------------------------------------------------------------------
// Payload decoding (the schema checks serde does not do)
// ---------------------------------------------------------------------------------------------

/// A payload that does not satisfy its schema: a per-request defect with the reason.
fn invalid<E>(reason: impl std::fmt::Display) -> Failure<E> {
    Failure::Die(serde_json::Value::String(reason.to_string()))
}

/// `TrimmedNonEmptyString.check(isMaxLength(max))`: the trimmed value.
fn trimmed<E>(field: &str, value: &mut String, max: usize) -> Result<(), Failure<E>> {
    let text = value.trim();
    if text.is_empty() {
        return Err(invalid(format!("Expected a non-empty value at [\"{field}\"]")));
    }
    if js_length(text) > max {
        return Err(invalid(format!("Expected a value with a length of at most {max} at [\"{field}\"]")));
    }
    *value = text.to_owned();
    Ok(())
}

fn size_limit<E>(value: i64, max: i64) -> Result<(), Failure<E>> {
    if !(1..=max).contains(&value) {
        return Err(invalid(format!("Expected a value between 1 and {max} at [\"sizeBytes\"], got {value}")));
    }
    Ok(())
}

fn validate_app<E>(app: &mut ToolActivityNativeAppReference) -> Result<(), Failure<E>> {
    match app {
        ToolActivityNativeAppReference::AppId(app) => {
            trimmed("app.appId", &mut app.app_id, 512)?;
            if !app.app_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-') {
                return Err(invalid("Expected a bundle identifier at [\"app\"][\"appId\"]"));
            }
        }
        ToolActivityNativeAppReference::DisplayName(app) => trimmed("app.displayName", &mut app.display_name, 160)?,
    }
    Ok(())
}

/// `AssetResource` decoding: trims and bounds every string.
pub fn validate_resource<E>(resource: &mut AssetResource) -> Result<(), Failure<E>> {
    match resource {
        AssetResource::WorkspaceFile(file) => trimmed("path", &mut file.path, ASSET_PATH_MAX_LENGTH),
        AssetResource::MediaFile(file) => trimmed("path", &mut file.path, ASSET_PATH_MAX_LENGTH),
        AssetResource::DraftWorkspaceFile(draft) => {
            trimmed("cwd", &mut draft.cwd, ASSET_PATH_MAX_LENGTH)?;
            trimmed("path", &mut draft.path, ASSET_PATH_MAX_LENGTH)
        }
        AssetResource::Attachment(attachment) => {
            trimmed("attachmentId", &mut attachment.attachment_id, 256)?;
            if let Some(file_name) = &mut attachment.file_name {
                trimmed("fileName", file_name, 255)?;
            }
            if let Some(mime_type) = &mut attachment.mime_type {
                trimmed("mimeType", mime_type, 100)?;
            }
            Ok(())
        }
        AssetResource::ProjectFavicon(favicon) => {
            trimmed("cwd", &mut favicon.cwd, ASSET_PATH_MAX_LENGTH)?;
            if let Some(path) = &mut favicon.path {
                trimmed("path", path, ASSET_PATH_MAX_LENGTH)?;
                let pattern = Regex::new(r"(?i)\.(?:avif|gif|ico|jpe?g|png|svg|webp)$").expect("static regex");
                if !pattern.is_match(path) {
                    return Err(invalid("Expected a favicon path at [\"path\"]"));
                }
            }
            Ok(())
        }
        AssetResource::NativeAppIcon(icon) => validate_app(&mut icon.app),
        AssetResource::GithubMedia(media) => {
            trimmed("cwd", &mut media.cwd, ASSET_PATH_MAX_LENGTH)?;
            trimmed("url", &mut media.url, 2048)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

fn context_resolution(resource: &AssetResource, error: &zc_ports::contracts::PersistenceError) -> AssetAccessError {
    AssetAccessError::AssetWorkspaceContextResolutionError(AssetWorkspaceContextResolutionError {
        tag: LitAssetWorkspaceContextResolutionError,
        resource: resource.clone(),
        cause: Defect::error(&error.tag, error.to_string()).0,
    })
}

/// The `assets.createUrl` handler of `ws.ts`.
pub async fn create_url(
    assets: &Assets,
    reads: &dyn ProjectionReads,
    mut input: AssetCreateUrlInput,
) -> Result<AssetCreateUrlResult, Failure<AssetAccessError>> {
    validate_resource(&mut input.resource)?;
    let resource = input.resource;
    let issue = |input: IssueAssetUrlInput| async move { assets.access.issue_asset_url(input).await.map_err(Failure::Fail) };
    let thread_id = match &resource {
        // An absolute media path can be linked from a thread on another environment; GitHub
        // media names the repository it authenticates through itself.
        AssetResource::Attachment(_) | AssetResource::NativeAppIcon(_) | AssetResource::GithubMedia(_) => {
            return issue(IssueAssetUrlInput::new(resource)).await
        }
        AssetResource::MediaFile(media) if is_absolute(&media.path) => return issue(IssueAssetUrlInput::new(resource)).await,
        AssetResource::DraftWorkspaceFile(draft) => {
            let cwd = draft.cwd.clone();
            return issue(IssueAssetUrlInput {
                workspace_root: Some(cwd),
                ..IssueAssetUrlInput::new(resource)
            })
            .await;
        }
        AssetResource::ProjectFavicon(favicon) => {
            let project = reads
                .get_active_project_by_workspace_root(&favicon.cwd)
                .await
                .map_err(|error| Failure::Fail(context_resolution(&resource, &error)))?
                .ok_or_else(|| Failure::Fail(errors::context_not_found(&resource)))?;
            let project_favicon_path = project.favicon_path.flatten().map(|path| path.to_string()).filter(|path| !path.is_empty());
            return issue(IssueAssetUrlInput {
                project_favicon_path,
                ..IssueAssetUrlInput::new(resource)
            })
            .await;
        }
        AssetResource::WorkspaceFile(file) => file.thread_id.clone(),
        AssetResource::MediaFile(media) => media.thread_id.clone(),
    };
    let thread = reads
        .get_thread_shell_by_id(&thread_id)
        .await
        .map_err(|error| Failure::Fail(context_resolution(&resource, &error)))?
        .ok_or_else(|| Failure::Fail(errors::context_not_found(&resource)))?;
    let project = reads
        .get_project_shell_by_id(&thread.project_id)
        .await
        .map_err(|error| Failure::Fail(context_resolution(&resource, &error)))?
        .ok_or_else(|| Failure::Fail(errors::context_not_found(&resource)))?;
    let workspace_root = thread
        .worktree_path
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| project.workspace_root.to_string());
    issue(IssueAssetUrlInput {
        workspace_root: Some(workspace_root),
        ..IssueAssetUrlInput::new(resource)
    })
    .await
}

/// The `attachments.createUploadUrl` handler.
pub async fn create_upload_url(
    assets: &Assets,
    mut input: AttachmentCreateUploadUrlInput,
) -> Result<AttachmentCreateUploadUrlResult, Failure<AttachmentUploadSigningKeyError>> {
    match &mut input {
        AttachmentCreateUploadUrlInput::Image(image) => {
            trimmed("name", &mut image.name, 255)?;
            size_limit(image.size_bytes, MAX_IMAGE_UPLOAD_BYTES)?;
        }
        AttachmentCreateUploadUrlInput::File(file) => {
            trimmed("name", &mut file.name, 255)?;
            trimmed("mimeType", &mut file.mime_type, 100)?;
            size_limit(file.size_bytes, MAX_FILE_UPLOAD_BYTES)?;
        }
    }
    assets.uploads.issue_upload_url(&input).await.map_err(Failure::Fail)
}

/// The `attachments.delete` handler.
pub async fn delete(assets: &Assets, mut input: AttachmentDeleteInput) -> Result<(), Failure<AttachmentUploadSigningKeyError>> {
    trimmed("attachmentId", &mut input.attachment_id, 256)?;
    assets.uploads.delete_pending_attachment(&input.attachment_id).await;
    Ok(())
}

/// Registers the three methods.
pub fn register(builder: RpcRouterBuilder, assets: Arc<Assets>, reads: Arc<dyn ProjectionReads>) -> RpcRouterBuilder {
    let create_url_assets = assets.clone();
    let upload_assets = assets.clone();
    let delete_assets = assets;
    builder
        .typed_unary_with::<AssetsCreateUrl, _, _>(zc_auth::rpc_method_options(AssetsCreateUrl::TAG), move |_ctx, input| {
            let assets = create_url_assets.clone();
            let reads = reads.clone();
            async move { create_url(&assets, reads.as_ref(), input).await }
        })
        .typed_unary_with::<AttachmentsCreateUploadUrl, _, _>(zc_auth::rpc_method_options(AttachmentsCreateUploadUrl::TAG), move |_ctx, input| {
            let assets = upload_assets.clone();
            async move { create_upload_url(&assets, input).await }
        })
        .typed_unary_with::<AttachmentsDelete, _, _>(zc_auth::rpc_method_options(AttachmentsDelete::TAG), move |_ctx, input| {
            let assets = delete_assets.clone();
            async move { delete(&assets, input).await }
        })
}
