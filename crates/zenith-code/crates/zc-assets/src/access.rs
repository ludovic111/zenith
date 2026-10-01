//! `AssetAccess.ts`: signed capability URLs for files the web app shows.
//!
//! ```text
//! /api/assets/<base64url(JSON claims)>.<base64url(HMAC-SHA256(key, first part))>/<encodeURIComponent(name)>
//! ```
//!
//! The key is `secrets/asset-access-signing-key.bin` (32 random bytes, shared with the TS
//! server, which is what makes a URL signed by one verify in the other). Claims carry
//! `version: 1`, a `kind` and `expiresAt` (epoch milliseconds, one hour after minting; project
//! favicons are bucketed per 30 minutes so a sidebar of icons keeps stable URLs):
//!
//! | kind | grants |
//! |---|---|
//! | `workspace-file` | an HTML/PDF entry and its sibling assets under `baseRelativePath` |
//! | `workspace-file-exact` | one workspace image |
//! | `media-file-exact` | one host file, pinned to its device and inode |
//! | `attachment` | one chat attachment by id (download/inline decided at mint time) |
//! | `project-favicon` | the favicon resolved inside a workspace (or none: the fallback marker) |
//! | `project-favicon-external` | a saved favicon outside the workspace |
//! | `native-app-icon` | a macOS app's icon |
//! | `github-media` | a GitHub-hosted picture or recording, fetched with the `gh` credential |
//!
//! The JSON keys are written in the TS schema order, so a token minted here is byte-for-byte
//! the one TS would mint from the same claims.

use std::sync::Arc;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use zc_auth::token::{base64url_encode, decode_payload_text, sign_payload, split_token, timing_safe_equal_base64url};
use zc_auth::SharedClock;
use zc_contracts::{
    AssetAccessError, AssetAttachmentNotFoundError, AssetCreateUrlResult, AssetGitHubMediaUrlValidationError, AssetPreviewTypeValidationError,
    AssetProjectFaviconInspectionError, AssetProjectFaviconNotFoundError, AssetProjectFaviconResolutionError, AssetResource,
    AssetResourceAttachmentDisposition, AssetSigningKeyLoadError, AssetWorkspaceAssetInspectionError, AssetWorkspaceAssetNotFoundError,
    AssetWorkspaceContextNotFoundError, AssetWorkspacePathValidationError, AssetWorkspaceResolutionError, AssetWorkspaceRootNormalizationError, JsNumber,
    LitAssetAttachmentNotFoundError, LitAssetGitHubMediaUrlValidationError, LitAssetPreviewTypeValidationError, LitAssetProjectFaviconInspectionError,
    LitAssetProjectFaviconNotFoundError, LitAssetProjectFaviconResolutionError, LitAssetSigningKeyLoadError, LitAssetWorkspaceAssetInspectionError,
    LitAssetWorkspaceAssetNotFoundError, LitAssetWorkspaceContextNotFoundError, LitAssetWorkspacePathValidationError, LitAssetWorkspaceResolutionError,
    LitAssetWorkspaceRootNormalizationError, ToolActivityNativeAppReference,
};
use zc_core::{Defect, ServerSecretStore};
use zc_orchestration::attachments::{parse_attachment_file_extension, resolve_attachment_path_by_id};
use zc_workspace::errors::{platform_error_defect, TaggedError};
use zc_workspace::paths::{resolve_relative_path_within_root, WorkspacePaths};

use crate::favicon::ProjectFaviconResolver;
use crate::image_dimensions::{read_image_dimensions, ImageDimensions, HEADER_IMAGE_EXTENSIONS, IMAGE_DIMENSIONS_HEADER_BYTES};
use crate::media_file::{open_media_file, FileIdentity, OpenMediaFile};
use crate::native_app_icon::NativeAppIconResolver;
use crate::preview::{
    audio_mime_type_from_extension, basename, decode_uri_component, dirname, encode_uri_component, extname, host_preview_mime_type_from_extension, is_absolute,
    is_preview_asset_extension, is_workspace_image_preview_path, is_workspace_preview_entry_path, join, normalize, relative, resolve,
    PROJECT_FAVICON_FALLBACK_MARKER,
};

pub const ASSET_ROUTE_PREFIX: &str = "/api/assets";
/// Shared by asset and upload tokens (their claim kinds differ).
pub const SIGNING_SECRET_NAME: &str = "asset-access-signing-key";
pub const ASSET_TOKEN_TTL_MS: i64 = 60 * 60 * 1000;
pub const PROJECT_FAVICON_TOKEN_BUCKET_MS: i64 = 30 * 60 * 1000;
const PROJECT_FAVICON_VERSION_PREFIX: &str = "v";

/// A JS number as JSON: integral values as integers (`JSON.stringify(1.7e12)` is `1700000000000`).
pub(crate) fn json_number(value: f64) -> Value {
    serde_json::to_value(JsNumber(value)).unwrap_or(Value::Null)
}

/// The signed claims (`AssetClaimsSchema`).
#[derive(Debug, Clone, PartialEq)]
pub enum AssetClaims {
    WorkspaceFile {
        workspace_root: String,
        base_relative_path: String,
        expires_at: f64,
    },
    WorkspaceFileExact {
        workspace_root: String,
        relative_path: String,
        expires_at: f64,
    },
    MediaFileExact {
        file_path: String,
        device: String,
        inode: String,
        expires_at: f64,
    },
    Attachment {
        attachment_id: String,
        download: Option<bool>,
        file_name: Option<String>,
        mime_type: Option<String>,
        expires_at: f64,
    },
    ProjectFavicon {
        workspace_root: String,
        relative_path: Option<String>,
        expires_at: f64,
    },
    ProjectFaviconExternal {
        file_path: String,
        expires_at: f64,
    },
    NativeAppIcon {
        app: ToolActivityNativeAppReference,
        expires_at: f64,
    },
    GithubMedia {
        url: String,
        cwd: String,
        expires_at: f64,
    },
}

impl AssetClaims {
    pub fn expires_at(&self) -> f64 {
        match self {
            Self::WorkspaceFile { expires_at, .. }
            | Self::WorkspaceFileExact { expires_at, .. }
            | Self::MediaFileExact { expires_at, .. }
            | Self::Attachment { expires_at, .. }
            | Self::ProjectFavicon { expires_at, .. }
            | Self::ProjectFaviconExternal { expires_at, .. }
            | Self::NativeAppIcon { expires_at, .. }
            | Self::GithubMedia { expires_at, .. } => *expires_at,
        }
    }

    fn set_expires_at(&mut self, value: f64) {
        match self {
            Self::WorkspaceFile { expires_at, .. }
            | Self::WorkspaceFileExact { expires_at, .. }
            | Self::MediaFileExact { expires_at, .. }
            | Self::Attachment { expires_at, .. }
            | Self::ProjectFavicon { expires_at, .. }
            | Self::ProjectFaviconExternal { expires_at, .. }
            | Self::NativeAppIcon { expires_at, .. }
            | Self::GithubMedia { expires_at, .. } => *expires_at = value,
        }
    }

    /// The claims' JSON, keys in schema order.
    pub fn to_json(&self) -> String {
        let mut map = Map::new();
        map.insert("version".into(), Value::from(1));
        let s = |value: &str| Value::String(value.to_owned());
        match self {
            Self::WorkspaceFile {
                workspace_root,
                base_relative_path,
                ..
            } => {
                map.insert("kind".into(), s("workspace-file"));
                map.insert("workspaceRoot".into(), s(workspace_root));
                map.insert("baseRelativePath".into(), s(base_relative_path));
            }
            Self::WorkspaceFileExact {
                workspace_root, relative_path, ..
            } => {
                map.insert("kind".into(), s("workspace-file-exact"));
                map.insert("workspaceRoot".into(), s(workspace_root));
                map.insert("relativePath".into(), s(relative_path));
            }
            Self::MediaFileExact { file_path, device, inode, .. } => {
                map.insert("kind".into(), s("media-file-exact"));
                map.insert("filePath".into(), s(file_path));
                map.insert("device".into(), s(device));
                map.insert("inode".into(), s(inode));
            }
            Self::Attachment {
                attachment_id,
                download,
                file_name,
                mime_type,
                ..
            } => {
                map.insert("kind".into(), s("attachment"));
                map.insert("attachmentId".into(), s(attachment_id));
                if let Some(download) = download {
                    map.insert("download".into(), Value::Bool(*download));
                }
                if let Some(file_name) = file_name {
                    map.insert("fileName".into(), s(file_name));
                }
                if let Some(mime_type) = mime_type {
                    map.insert("mimeType".into(), s(mime_type));
                }
            }
            Self::ProjectFavicon {
                workspace_root, relative_path, ..
            } => {
                map.insert("kind".into(), s("project-favicon"));
                map.insert("workspaceRoot".into(), s(workspace_root));
                map.insert("relativePath".into(), relative_path.as_deref().map(s).unwrap_or(Value::Null));
            }
            Self::ProjectFaviconExternal { file_path, .. } => {
                map.insert("kind".into(), s("project-favicon-external"));
                map.insert("filePath".into(), s(file_path));
            }
            Self::NativeAppIcon { app, .. } => {
                map.insert("kind".into(), s("native-app-icon"));
                map.insert("app".into(), serde_json::to_value(app).unwrap_or(Value::Null));
            }
            Self::GithubMedia { url, cwd, .. } => {
                map.insert("kind".into(), s("github-media"));
                map.insert("url".into(), s(url));
                map.insert("cwd".into(), s(cwd));
            }
        }
        map.insert("expiresAt".into(), json_number(self.expires_at()));
        Value::Object(map).to_string()
    }

    /// `Schema.decodeUnknownOption(Schema.fromJsonString(AssetClaimsSchema))`.
    pub fn from_json(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        let object = value.as_object()?;
        if object.get("version")?.as_f64()? != 1.0 {
            return None;
        }
        let string = |key: &str| object.get(key).and_then(Value::as_str).map(str::to_owned);
        let optional_string = |key: &str| -> Option<Option<String>> {
            match object.get(key) {
                None => Some(None),
                Some(Value::String(text)) => Some(Some(text.clone())),
                Some(_) => None,
            }
        };
        let expires_at = object.get("expiresAt")?.as_f64()?;
        Some(match object.get("kind")?.as_str()? {
            "workspace-file" => Self::WorkspaceFile {
                workspace_root: string("workspaceRoot")?,
                base_relative_path: string("baseRelativePath")?,
                expires_at,
            },
            "workspace-file-exact" => Self::WorkspaceFileExact {
                workspace_root: string("workspaceRoot")?,
                relative_path: string("relativePath")?,
                expires_at,
            },
            "media-file-exact" => Self::MediaFileExact {
                file_path: string("filePath")?,
                device: string("device")?,
                inode: string("inode")?,
                expires_at,
            },
            "attachment" => Self::Attachment {
                attachment_id: string("attachmentId")?,
                download: match object.get("download") {
                    None => None,
                    Some(Value::Bool(flag)) => Some(*flag),
                    Some(_) => return None,
                },
                file_name: optional_string("fileName")?,
                mime_type: optional_string("mimeType")?,
                expires_at,
            },
            "project-favicon" => Self::ProjectFavicon {
                workspace_root: string("workspaceRoot")?,
                relative_path: match object.get("relativePath")? {
                    Value::Null => None,
                    Value::String(text) => Some(text.clone()),
                    _ => return None,
                },
                expires_at,
            },
            "project-favicon-external" => Self::ProjectFaviconExternal {
                file_path: string("filePath")?,
                expires_at,
            },
            "native-app-icon" => Self::NativeAppIcon {
                app: serde_json::from_value(object.get("app")?.clone()).ok()?,
                expires_at,
            },
            "github-media" => Self::GithubMedia {
                url: string("url")?,
                cwd: string("cwd")?,
                expires_at,
            },
            _ => return None,
        })
    }
}

/// Signs encoded claims: `<payload>.<signature>`.
pub fn sign_token(claims_json: &str, secret: &[u8]) -> String {
    let payload = base64url_encode(claims_json.as_bytes());
    let signature = sign_payload(&payload, secret);
    format!("{payload}.{signature}")
}

/// The payload of a token whose signature checks out (first two `.` parts).
pub fn verified_payload(token: &str, secret: &[u8]) -> Option<String> {
    let (payload, signature) = split_token(token)?;
    if !timing_safe_equal_base64url(signature, &sign_payload(payload, secret)) {
        return None;
    }
    decode_payload_text(payload)
}

/// What a signed URL resolves to.
#[derive(Debug)]
pub enum ResolvedAsset {
    File(ResolvedFile),
    GithubMedia { url: String, cwd: String, expires_at: f64 },
}

/// A file to serve.
#[derive(Debug, Default)]
pub struct ResolvedFile {
    pub path: String,
    pub download: bool,
    pub file_name: Option<String>,
    pub mime_type: Option<String>,
    /// The identity-checked descriptor of a `media-file-exact` URL.
    pub file: Option<OpenMediaFile>,
}

impl ResolvedFile {
    fn at(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            ..Self::default()
        }
    }
}

/// `issueAssetUrl` input: the resource, the workspace it resolves against, and the project's
/// saved favicon path.
#[derive(Debug, Clone)]
pub struct IssueAssetUrlInput {
    pub resource: AssetResource,
    pub workspace_root: Option<String>,
    pub project_favicon_path: Option<String>,
}

impl IssueAssetUrlInput {
    pub fn new(resource: AssetResource) -> Self {
        Self {
            resource,
            workspace_root: None,
            project_favicon_path: None,
        }
    }
}

/// The asset access service.
#[derive(Clone)]
pub struct AssetAccess {
    pub(crate) attachments_dir: String,
    pub(crate) secrets: ServerSecretStore,
    pub(crate) clock: SharedClock,
    pub(crate) favicons: ProjectFaviconResolver,
    pub(crate) native_icons: NativeAppIconResolver,
    workspace_paths: WorkspacePaths,
}

impl std::fmt::Debug for AssetAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssetAccess")
            .field("attachments_dir", &self.attachments_dir)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------------------------
// File-system helpers
// ---------------------------------------------------------------------------------------------

fn not_found_as_none<T>(result: std::io::Result<T>) -> std::io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// A file-system failure as a `PlatformError` defect.
fn fs_defect(error: &std::io::Error, method: &str, path: &str) -> Defect {
    let syscall = match method {
        "realPath" => "realpath",
        "readFile" => "open",
        _ => "stat",
    };
    platform_error_defect(error, method, syscall, path)
}

fn realpath(path: &str) -> Result<Option<String>, Defect> {
    not_found_as_none(std::fs::canonicalize(path))
        .map(|real| real.map(|real| real.to_string_lossy().into_owned()))
        .map_err(|error| fs_defect(&error, "realPath", path))
}

fn is_regular_file(path: &str) -> Result<bool, Defect> {
    not_found_as_none(std::fs::metadata(path))
        .map(|metadata| metadata.is_some_and(|m| m.is_file()))
        .map_err(|error| fs_defect(&error, "stat", path))
}

/// `resolveCanonicalFile`: the canonical path of an existing regular file.
fn resolve_canonical_file(file_path: &str) -> Result<Option<String>, Defect> {
    let Some(canonical) = realpath(file_path)? else { return Ok(None) };
    Ok(is_regular_file(&canonical)?.then_some(canonical))
}

/// `resolveCanonicalWorkspaceFile`: a regular file strictly inside the canonical workspace root.
fn resolve_canonical_workspace_file(workspace_root: &str, relative_path: &str) -> Result<Option<String>, Defect> {
    let Ok(resolved) = resolve_relative_path_within_root(workspace_root, relative_path) else {
        return Ok(None);
    };
    let canonical_root = realpath(workspace_root)?;
    let canonical_file = realpath(&resolved.absolute_path)?;
    let (Some(canonical_root), Some(canonical_file)) = (canonical_root, canonical_file) else {
        return Ok(None);
    };
    let inner = relative(&canonical_root, &canonical_file);
    if inner.is_empty() || inner.starts_with("..") || is_absolute(&inner) {
        return Ok(None);
    }
    Ok(is_regular_file(&canonical_file)?.then_some(canonical_file))
}

/// `resolveCanonicalWorkspaceFileForRequest`: failures are logged and serve nothing.
fn resolve_canonical_workspace_file_for_request(workspace_root: &str, relative_path: &str) -> Option<String> {
    resolve_canonical_workspace_file(workspace_root, relative_path).unwrap_or_else(|cause| {
        tracing::error!(workspace_root, relative_path, %cause, "Failed to resolve canonical asset path.");
        None
    })
}

fn wants_header_dimensions(path: &str) -> bool {
    HEADER_IMAGE_EXTENSIONS.contains(&extname(path).to_lowercase().as_str())
}

/// `readImageDimensionsFromOpenFile`.
fn dimensions_of_open_file(file: &OpenMediaFile) -> Option<ImageDimensions> {
    file.read_header(IMAGE_DIMENSIONS_HEADER_BYTES)
        .ok()
        .and_then(|bytes| read_image_dimensions(&bytes))
}

/// `readImageDimensionsFromHeader`: opened like media, so a FIFO cannot block.
fn dimensions_from_header(file_path: &str) -> Option<ImageDimensions> {
    open_media_file(file_path, None).ok().flatten().and_then(|file| dimensions_of_open_file(&file))
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// The error constructors, each carrying the requested resource.
pub(crate) mod errors {
    use super::*;

    pub fn context_not_found(resource: &AssetResource) -> AssetAccessError {
        AssetAccessError::AssetWorkspaceContextNotFoundError(AssetWorkspaceContextNotFoundError {
            tag: LitAssetWorkspaceContextNotFoundError,
            resource: resource.clone(),
        })
    }
    pub fn root_normalization(resource: &AssetResource, cause: Defect) -> AssetAccessError {
        AssetAccessError::AssetWorkspaceRootNormalizationError(AssetWorkspaceRootNormalizationError {
            tag: LitAssetWorkspaceRootNormalizationError,
            resource: resource.clone(),
            cause: cause.0,
        })
    }
    pub fn path_validation(resource: &AssetResource, cause: Defect) -> AssetAccessError {
        AssetAccessError::AssetWorkspacePathValidationError(AssetWorkspacePathValidationError {
            tag: LitAssetWorkspacePathValidationError,
            resource: resource.clone(),
            cause: cause.0,
        })
    }
    pub fn preview_type(resource: &AssetResource) -> AssetAccessError {
        AssetAccessError::AssetPreviewTypeValidationError(AssetPreviewTypeValidationError {
            tag: LitAssetPreviewTypeValidationError,
            resource: resource.clone(),
        })
    }
    pub fn inspection(resource: &AssetResource, cause: Defect) -> AssetAccessError {
        AssetAccessError::AssetWorkspaceAssetInspectionError(AssetWorkspaceAssetInspectionError {
            tag: LitAssetWorkspaceAssetInspectionError,
            resource: resource.clone(),
            cause: cause.0,
        })
    }
    pub fn asset_not_found(resource: &AssetResource) -> AssetAccessError {
        AssetAccessError::AssetWorkspaceAssetNotFoundError(AssetWorkspaceAssetNotFoundError {
            tag: LitAssetWorkspaceAssetNotFoundError,
            resource: resource.clone(),
        })
    }
    pub fn workspace_resolution(resource: &AssetResource, cause: Defect) -> AssetAccessError {
        AssetAccessError::AssetWorkspaceResolutionError(AssetWorkspaceResolutionError {
            tag: LitAssetWorkspaceResolutionError,
            resource: resource.clone(),
            cause: cause.0,
        })
    }
    pub fn attachment_not_found(resource: &AssetResource) -> AssetAccessError {
        AssetAccessError::AssetAttachmentNotFoundError(AssetAttachmentNotFoundError {
            tag: LitAssetAttachmentNotFoundError,
            resource: resource.clone(),
        })
    }
    pub fn favicon_resolution(resource: &AssetResource, cause: Defect) -> AssetAccessError {
        AssetAccessError::AssetProjectFaviconResolutionError(AssetProjectFaviconResolutionError {
            tag: LitAssetProjectFaviconResolutionError,
            resource: resource.clone(),
            cause: cause.0,
        })
    }
    pub fn favicon_inspection(resource: &AssetResource, cause: Defect) -> AssetAccessError {
        AssetAccessError::AssetProjectFaviconInspectionError(AssetProjectFaviconInspectionError {
            tag: LitAssetProjectFaviconInspectionError,
            resource: resource.clone(),
            cause: cause.0,
        })
    }
    pub fn favicon_not_found(resource: &AssetResource) -> AssetAccessError {
        AssetAccessError::AssetProjectFaviconNotFoundError(AssetProjectFaviconNotFoundError {
            tag: LitAssetProjectFaviconNotFoundError,
            resource: resource.clone(),
        })
    }
    pub fn github_media_url() -> AssetAccessError {
        AssetAccessError::AssetGitHubMediaUrlValidationError(AssetGitHubMediaUrlValidationError {
            tag: LitAssetGitHubMediaUrlValidationError,
        })
    }
    pub fn signing_key(resource: &AssetResource, cause: Defect) -> AssetAccessError {
        AssetAccessError::AssetSigningKeyLoadError(AssetSigningKeyLoadError {
            tag: LitAssetSigningKeyLoadError,
            resource: resource.clone(),
            cause: cause.0,
        })
    }
}

/// What issuing found, before signing.
struct Minted {
    claims: AssetClaims,
    file_name: String,
    source_path: Option<String>,
    image_dimensions: Option<ImageDimensions>,
}

/// `/^video\/[\w!#$&^.+-]+$/i`.
fn is_inline_video_mime_type(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower
        .strip_prefix("video/")
        .is_some_and(|subtype| !subtype.is_empty() && subtype.bytes().all(|b| b.is_ascii_alphanumeric() || b"_!#$&^.+-".contains(&b)))
}

/// `INLINE_PREVIEW_MIME_TYPES` then audio: what a viewer may request inline, by the extension
/// the server assigned.
fn inline_preview_mime_type_for_extension(extension: &str) -> Option<String> {
    match extension {
        "pdf" => Some("application/pdf".into()),
        "html" | "htm" => Some("text/html".into()),
        _ => audio_mime_type_from_extension(&format!(".{extension}")).map(str::to_owned),
    }
}

impl AssetAccess {
    pub fn new(
        attachments_dir: &std::path::Path,
        secrets: ServerSecretStore,
        clock: SharedClock,
        favicons: ProjectFaviconResolver,
        native_icons: NativeAppIconResolver,
    ) -> Self {
        Self {
            attachments_dir: attachments_dir.to_string_lossy().into_owned(),
            secrets,
            clock,
            favicons,
            native_icons,
            workspace_paths: WorkspacePaths::new(),
        }
    }

    pub fn favicons(&self) -> &ProjectFaviconResolver {
        &self.favicons
    }

    fn normalize_root(&self, resource: &AssetResource, root: &str) -> Result<String, AssetAccessError> {
        self.workspace_paths
            .normalize_workspace_root(root, false)
            .map_err(|error| errors::root_normalization(resource, error.to_defect()))
    }

    /// `finalizeAbsoluteMediaFileAsset`.
    fn finalize_absolute_media(&self, resource: &AssetResource, requested_path: &str, expires_at: f64) -> Result<Minted, AssetAccessError> {
        let canonical = resolve_canonical_file(requested_path)
            .map_err(|cause| errors::inspection(resource, cause))?
            .ok_or_else(|| errors::asset_not_found(resource))?;
        if host_preview_mime_type_from_extension(extname(&canonical)).is_none() {
            return Err(errors::preview_type(resource));
        }
        let opened = open_media_file(&canonical, None)
            .map_err(|error| errors::inspection(resource, fs_defect(&error, "open", &canonical)))?
            .ok_or_else(|| errors::asset_not_found(resource))?;
        let image_dimensions = if wants_header_dimensions(&canonical) {
            dimensions_of_open_file(&opened)
        } else {
            None
        };
        let FileIdentity { device, inode } = opened.identity();
        Ok(Minted {
            file_name: basename(&canonical),
            claims: AssetClaims::MediaFileExact {
                file_path: canonical,
                device,
                inode,
                expires_at,
            },
            source_path: None,
            image_dimensions,
        })
    }

    /// `finalizeWorkspaceFileAsset`.
    fn finalize_workspace_file(
        &self,
        resource: &AssetResource,
        workspace_root: &str,
        requested_path: &str,
        expires_at: f64,
    ) -> Result<Minted, AssetAccessError> {
        let relative_path = if is_absolute(requested_path) {
            relative(workspace_root, requested_path)
        } else {
            requested_path.to_owned()
        };
        let resolved =
            resolve_relative_path_within_root(workspace_root, &relative_path).map_err(|error| errors::path_validation(resource, error.to_defect()))?;
        if !is_workspace_preview_entry_path(&resolved.relative_path) {
            return Err(errors::preview_type(resource));
        }
        let canonical = resolve_canonical_workspace_file(workspace_root, &resolved.relative_path)
            .map_err(|cause| errors::inspection(resource, cause))?
            .ok_or_else(|| errors::asset_not_found(resource))?;
        let canonical_root = std::fs::canonicalize(workspace_root)
            .map(|real| real.to_string_lossy().into_owned())
            .map_err(|error| errors::workspace_resolution(resource, fs_defect(&error, "realPath", workspace_root)))?;
        let image_dimensions = if wants_header_dimensions(&resolved.relative_path) {
            dimensions_from_header(&canonical)
        } else {
            None
        };
        let claims = if is_workspace_image_preview_path(&resolved.relative_path) {
            AssetClaims::WorkspaceFileExact {
                workspace_root: canonical_root,
                relative_path: resolved.relative_path.clone(),
                expires_at,
            }
        } else {
            AssetClaims::WorkspaceFile {
                workspace_root: canonical_root,
                base_relative_path: dirname(&resolved.relative_path),
                expires_at,
            }
        };
        Ok(Minted {
            claims,
            file_name: basename(&resolved.relative_path),
            source_path: None,
            image_dimensions,
        })
    }

    /// The claims for `input` (blocking file-system work).
    fn mint(&self, input: &IssueAssetUrlInput, expires_at: f64) -> Result<Minted, AssetAccessError> {
        let resource = &input.resource;
        match resource {
            AssetResource::MediaFile(media) => {
                let mut requested_path = media.path.clone();
                if !is_absolute(&requested_path) {
                    let Some(root) = &input.workspace_root else {
                        return Err(errors::context_not_found(resource));
                    };
                    let root = self.normalize_root(resource, root)?;
                    requested_path = resolve(&root, &requested_path);
                }
                self.finalize_absolute_media(resource, &requested_path, expires_at)
            }
            AssetResource::WorkspaceFile(file) => {
                let Some(root) = &input.workspace_root else {
                    return Err(errors::context_not_found(resource));
                };
                let root = self.normalize_root(resource, root)?;
                self.finalize_workspace_file(resource, &root, &file.path, expires_at)
            }
            AssetResource::DraftWorkspaceFile(draft) => {
                if is_absolute(&draft.path) {
                    return self.finalize_absolute_media(resource, &draft.path, expires_at);
                }
                let root = self.normalize_root(resource, input.workspace_root.as_deref().unwrap_or(&draft.cwd))?;
                self.finalize_workspace_file(resource, &root, &draft.path, expires_at)
            }
            AssetResource::Attachment(attachment) => {
                let attachment_path = resolve_attachment_path_by_id(std::path::Path::new(&self.attachments_dir), &attachment.attachment_id)
                    .ok_or_else(|| errors::attachment_not_found(resource))?
                    .to_string_lossy()
                    .into_owned();
                let extension = parse_attachment_file_extension(&attachment.attachment_id);
                let is_generic_file = extension.is_some();
                let video_mime_type = attachment
                    .mime_type
                    .as_deref()
                    .map(|mime| mime.split(';').next().unwrap_or("").trim().to_owned())
                    .unwrap_or_default();
                let is_video = is_inline_video_mime_type(&video_mime_type);
                let inline_preview_mime_type = match (&attachment.disposition, &extension) {
                    (Some(AssetResourceAttachmentDisposition::Inline), Some(extension)) => inline_preview_mime_type_for_extension(extension),
                    _ => None,
                };
                let image_dimensions = if is_generic_file { None } else { dimensions_from_header(&attachment_path) };
                let mime_type = inline_preview_mime_type.clone().or_else(|| {
                    attachment
                        .mime_type
                        .as_ref()
                        .map(|mime| if is_video { video_mime_type.clone() } else { mime.clone() })
                });
                Ok(Minted {
                    claims: AssetClaims::Attachment {
                        attachment_id: attachment.attachment_id.clone(),
                        download: (is_generic_file && !is_video && inline_preview_mime_type.is_none()).then_some(true),
                        file_name: attachment.file_name.clone(),
                        mime_type,
                        expires_at,
                    },
                    file_name: attachment.file_name.clone().unwrap_or_else(|| basename(&attachment_path)),
                    source_path: None,
                    image_dimensions,
                })
            }
            AssetResource::ProjectFavicon(favicon) => self.mint_project_favicon(resource, &favicon.cwd, input.project_favicon_path.as_deref(), expires_at),
            AssetResource::NativeAppIcon(icon) => Ok(Minted {
                claims: AssetClaims::NativeAppIcon {
                    app: icon.app.clone(),
                    expires_at,
                },
                file_name: "native-app-icon.png".into(),
                source_path: None,
                image_dimensions: None,
            }),
            AssetResource::GithubMedia(media) => {
                let fetch_url = crate::github_media::github_media_fetch_url(&media.url).ok_or_else(errors::github_media_url)?;
                Ok(Minted {
                    file_name: crate::github_media::github_media_file_name(&fetch_url),
                    claims: AssetClaims::GithubMedia {
                        url: fetch_url,
                        cwd: media.cwd.clone(),
                        expires_at,
                    },
                    source_path: None,
                    image_dimensions: None,
                })
            }
        }
    }

    fn mint_project_favicon(
        &self,
        resource: &AssetResource,
        cwd: &str,
        project_favicon_path: Option<&str>,
        expires_at: f64,
    ) -> Result<Minted, AssetAccessError> {
        let workspace_root = self.normalize_root(resource, cwd)?;
        let favicon_path = self
            .favicons
            .resolve_path_blocking(&workspace_root, project_favicon_path)
            .map_err(|error| errors::favicon_resolution(resource, error.to_defect()))?;
        let is_external_override = match (&favicon_path, project_favicon_path) {
            (Some(found), Some(saved)) => is_absolute(saved) && normalize(found) == normalize(saved),
            _ => false,
        };
        let relative_path = favicon_path
            .as_ref()
            .filter(|_| !is_external_override)
            .map(|found| relative(&workspace_root, found));
        let source_path = if is_external_override { favicon_path.clone() } else { relative_path.clone() };
        if let Some(source) = &source_path {
            if !is_workspace_image_preview_path(source) {
                return Err(errors::preview_type(resource));
            }
        }
        let canonical = match &source_path {
            Some(source) if is_external_override => resolve_canonical_file(source),
            Some(source) => resolve_canonical_workspace_file(&workspace_root, source),
            None => Ok(None),
        }
        .map_err(|cause| errors::favicon_inspection(resource, cause))?;
        if source_path.is_some() && canonical.is_none() {
            return Err(errors::favicon_not_found(resource));
        }
        let claims = match (&canonical, is_external_override) {
            (Some(canonical), true) => AssetClaims::ProjectFaviconExternal {
                file_path: canonical.clone(),
                expires_at,
            },
            _ => AssetClaims::ProjectFavicon {
                workspace_root: std::fs::canonicalize(&workspace_root)
                    .map(|real| real.to_string_lossy().into_owned())
                    .map_err(|error| errors::workspace_resolution(resource, fs_defect(&error, "realPath", &workspace_root)))?,
                relative_path,
                expires_at,
            },
        };
        let file_name = match (&source_path, &canonical) {
            (Some(source), Some(canonical)) => {
                let bytes = std::fs::read(canonical).map_err(|error| errors::favicon_inspection(resource, fs_defect(&error, "readFile", canonical)))?;
                let revision: String = Sha256::digest(&bytes).iter().map(|byte| format!("{byte:02x}")).collect();
                format!("{PROJECT_FAVICON_VERSION_PREFIX}{revision}-{}", basename(source))
            }
            _ => PROJECT_FAVICON_FALLBACK_MARKER.to_owned(),
        };
        Ok(Minted {
            claims,
            file_name,
            source_path,
            image_dimensions: None,
        })
    }

    async fn signing_secret(&self) -> Result<Vec<u8>, Defect> {
        self.secrets
            .get_or_create_random(SIGNING_SECRET_NAME, 32)
            .await
            .map_err(|error| Defect::from_error(&error))
    }

    /// `issueAssetUrl`.
    pub async fn issue_asset_url(&self, input: IssueAssetUrlInput) -> Result<AssetCreateUrlResult, AssetAccessError> {
        let expires_at = (self.clock.now_millis() + ASSET_TOKEN_TTL_MS) as f64;
        let this = self.clone();
        let resource = input.resource.clone();
        let mut minted = tokio::task::spawn_blocking(move || this.mint(&input, expires_at))
            .await
            .map_err(|error| errors::inspection(&resource, Defect::error("Error", error.to_string())))??;
        let secret = self.signing_secret().await.map_err(|cause| errors::signing_key(&resource, cause))?;
        let mut expires_at = minted.claims.expires_at();
        if matches!(minted.claims, AssetClaims::ProjectFavicon { .. } | AssetClaims::ProjectFaviconExternal { .. }) {
            let issued_at = self.clock.now_millis();
            expires_at = ((issued_at.div_euclid(PROJECT_FAVICON_TOKEN_BUCKET_MS) + 2) * PROJECT_FAVICON_TOKEN_BUCKET_MS) as f64;
            minted.claims.set_expires_at(expires_at);
        }
        let token = sign_token(&minted.claims.to_json(), &secret);
        Ok(AssetCreateUrlResult {
            relative_url: format!("{ASSET_ROUTE_PREFIX}/{token}/{}", encode_uri_component(&minted.file_name)),
            expires_at: JsNumber(expires_at),
            source_path: minted.source_path,
            image_dimensions: minted.image_dimensions.map(Into::into),
        })
    }

    /// The verified, unexpired claims of a token.
    pub async fn verify_token(&self, token: &str) -> Option<AssetClaims> {
        let secret = match self.signing_secret().await {
            Ok(secret) => secret,
            Err(cause) => {
                tracing::error!(%cause, "Failed to load the asset signing key.");
                return None;
            }
        };
        let claims = AssetClaims::from_json(&verified_payload(token, &secret)?)?;
        (claims.expires_at() > self.clock.now_millis() as f64).then_some(claims)
    }

    /// `resolveAsset`: what `GET /api/assets/<token>/<relative_path>` serves, or `None` (404).
    pub async fn resolve_asset(&self, token: &str, relative_path: &str) -> Option<ResolvedAsset> {
        let claims = self.verify_token(token).await?;
        match claims {
            AssetClaims::GithubMedia { url, cwd, expires_at } => return Some(ResolvedAsset::GithubMedia { url, cwd, expires_at }),
            AssetClaims::NativeAppIcon { app, .. } => return self.native_icons.resolve(&app).await.map(|path| ResolvedAsset::File(ResolvedFile::at(path))),
            _ => {}
        }
        let this = self.clone();
        let relative_path = relative_path.to_owned();
        tokio::task::spawn_blocking(move || this.resolve_file_blocking(claims, &relative_path))
            .await
            .ok()
            .flatten()
            .map(ResolvedAsset::File)
    }

    fn resolve_file_blocking(&self, claims: AssetClaims, relative_path: &str) -> Option<ResolvedFile> {
        match claims {
            AssetClaims::Attachment {
                attachment_id,
                download,
                file_name,
                mime_type,
                ..
            } => {
                let path = resolve_attachment_path_by_id(std::path::Path::new(&self.attachments_dir), &attachment_id)?
                    .to_string_lossy()
                    .into_owned();
                let is_file = not_found_as_none(std::fs::metadata(&path)).unwrap_or_else(|error| {
                    tracing::error!(attachment_id, path, %error, "Failed to inspect attachment asset.");
                    None
                });
                return is_file.is_some_and(|m| m.is_file()).then_some(ResolvedFile {
                    path,
                    download: download == Some(true),
                    file_name,
                    mime_type,
                    file: None,
                });
            }
            AssetClaims::ProjectFavicon {
                workspace_root, relative_path, ..
            } => {
                return resolve_canonical_workspace_file_for_request(&workspace_root, &relative_path?).map(ResolvedFile::at);
            }
            AssetClaims::ProjectFaviconExternal { file_path, .. } => {
                let canonical = resolve_canonical_file(&file_path).unwrap_or_else(|cause| {
                    tracing::error!(file_path, %cause, "Failed to resolve canonical asset path.");
                    None
                })?;
                return (canonical == file_path).then(|| ResolvedFile::at(canonical));
            }
            _ => {}
        }
        let decoded = decode_uri_component(relative_path)?;
        match claims {
            AssetClaims::MediaFileExact { file_path, device, inode, .. } => {
                if decoded != basename(&file_path) {
                    return None;
                }
                let canonical = resolve_canonical_file(&file_path).unwrap_or_else(|cause| {
                    tracing::error!(file_path, %cause, "Failed to resolve canonical media path.");
                    None
                })?;
                if canonical != file_path {
                    return None;
                }
                let mime_type = host_preview_mime_type_from_extension(extname(&canonical))?;
                let file = open_media_file(&canonical, Some(&FileIdentity { device, inode })).unwrap_or_else(|error| {
                    tracing::error!(file_path = canonical, %error, "Failed to open canonical media file.");
                    None
                })?;
                Some(ResolvedFile {
                    path: canonical,
                    mime_type: Some(mime_type),
                    file: Some(file),
                    ..ResolvedFile::default()
                })
            }
            AssetClaims::WorkspaceFileExact {
                workspace_root, relative_path, ..
            } => {
                if decoded != basename(&relative_path) {
                    return None;
                }
                resolve_canonical_workspace_file_for_request(&workspace_root, &relative_path).map(ResolvedFile::at)
            }
            AssetClaims::WorkspaceFile {
                workspace_root,
                base_relative_path,
                ..
            } => {
                let unsafe_segment = decoded
                    .split(['/', '\\'])
                    .any(|segment| segment == "." || segment == ".." || segment.starts_with('.'));
                if decoded.is_empty() || decoded.contains('\0') || unsafe_segment || !is_preview_asset_extension(extname(&decoded)) {
                    return None;
                }
                let joined = if base_relative_path == "." {
                    decoded
                } else {
                    join(&base_relative_path, &decoded)
                };
                resolve_canonical_workspace_file_for_request(&workspace_root, &joined).map(ResolvedFile::at)
            }
            _ => None,
        }
    }
}

/// A shared handle.
pub type SharedAssetAccess = Arc<AssetAccess>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_round_trip_in_schema_key_order() {
        let claims = AssetClaims::Attachment {
            attachment_id: "thread-1-00000000-0000-4000-8000-000000000001-pdf".into(),
            download: Some(true),
            file_name: Some("report.pdf".into()),
            mime_type: Some("application/pdf".into()),
            expires_at: 1_790_000_000_000.0,
        };
        let json = claims.to_json();
        assert_eq!(
            json,
            r#"{"version":1,"kind":"attachment","attachmentId":"thread-1-00000000-0000-4000-8000-000000000001-pdf","download":true,"fileName":"report.pdf","mimeType":"application/pdf","expiresAt":1790000000000}"#
        );
        assert_eq!(AssetClaims::from_json(&json), Some(claims));
        let favicon = AssetClaims::ProjectFavicon {
            workspace_root: "/w".into(),
            relative_path: None,
            expires_at: 3.0,
        };
        assert_eq!(
            favicon.to_json(),
            r#"{"version":1,"kind":"project-favicon","workspaceRoot":"/w","relativePath":null,"expiresAt":3}"#
        );
        assert_eq!(AssetClaims::from_json(&favicon.to_json()), Some(favicon));
        assert_eq!(
            AssetClaims::from_json(r#"{"version":2,"kind":"github-media","url":"u","cwd":"c","expiresAt":1}"#),
            None
        );
        assert_eq!(
            AssetClaims::from_json(r#"{"version":1,"kind":"attachment","attachmentId":"a","download":"yes","expiresAt":1}"#),
            None
        );
        assert_eq!(
            AssetClaims::from_json(r#"{"version":1,"kind":"project-favicon","workspaceRoot":"/w","expiresAt":1}"#),
            None
        );
    }

    #[test]
    fn tokens_verify_with_the_same_key_only() {
        let token = sign_token(r#"{"a":1}"#, b"key");
        assert_eq!(verified_payload(&token, b"key").as_deref(), Some(r#"{"a":1}"#));
        assert_eq!(verified_payload(&token, b"other"), None);
        assert_eq!(verified_payload(&format!("{token}tampered"), b"key"), None);
        assert_eq!(verified_payload("garbage", b"key"), None);
    }

    #[test]
    fn recognizes_inline_video_types() {
        assert!(is_inline_video_mime_type("VIDEO/mp4"));
        assert!(!is_inline_video_mime_type("video/"));
        assert!(!is_inline_video_mime_type("video/mp4 x"));
    }
}
