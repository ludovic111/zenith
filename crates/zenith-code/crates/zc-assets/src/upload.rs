//! `AttachmentUpload.ts`: signed upload URLs for chat attachments.
//!
//! `attachments.createUploadUrl` mints a pending attachment id and a URL valid for 10 minutes,
//! `POST /api/attachments/upload/<token>` streams exactly `sizeBytes` bytes into
//! `attachments/<id><ext>` (through a `.part` file renamed into place), and
//! `attachments.delete` removes a pending upload the composer dropped. Stale pending uploads
//! are swept when a URL is minted, at most every 15 minutes.
//!
//! ```text
//! token = base64url({"version":1,"kind":"attachment-upload","type","attachmentId","name","mimeType","sizeBytes","expiresAt"})
//!         "." base64url(HMAC-SHA256(asset-access-signing-key, first part))
//! ```

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use futures::{Stream, StreamExt as _};
use serde_json::{Map, Value};
use tokio::io::AsyncWriteExt as _;
use zc_auth::token::{decode_payload_text, sign_payload, timing_safe_equal_base64url};
use zc_auth::SharedClock;
use zc_contracts::{
    AttachmentCreateUploadUrlInput, AttachmentCreateUploadUrlResult, AttachmentUploadSigningKeyError, JsNumber, LitAttachmentUploadSigningKeyError,
};
use zc_core::{Defect, ServerSecretStore};
use zc_orchestration::attachments::{
    attachment_file_extension, create_pending_attachment_id, infer_image_extension, parse_thread_segment_from_attachment_id, resolve_attachment_path_by_id,
    resolve_attachment_relative_path, sweep_stale_pending_attachments, PENDING_ATTACHMENT_THREAD_SEGMENT,
};

use crate::access::{json_number, sign_token, SIGNING_SECRET_NAME};

pub const ATTACHMENT_UPLOAD_ROUTE_PREFIX: &str = "/api/attachments/upload";
/// `ATTACHMENT_UPLOAD_URL_TTL_MS`.
pub const ATTACHMENT_UPLOAD_URL_TTL_MS: i64 = 10 * 60_000;
const PENDING_ATTACHMENT_SWEEP_INTERVAL_MS: i64 = 15 * 60_000;

/// `type`: an image (the default, also for tokens minted before file support) or any file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadType {
    Image,
    File,
}

impl UploadType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::File => "file",
        }
    }
}

/// `AttachmentUploadClaims`.
#[derive(Debug, Clone, PartialEq)]
pub struct AttachmentUploadClaims {
    pub r#type: UploadType,
    pub attachment_id: String,
    pub name: String,
    pub mime_type: String,
    pub size_bytes: f64,
    pub expires_at: f64,
}

impl AttachmentUploadClaims {
    /// The claims' JSON, keys in schema order.
    pub fn to_json(&self) -> String {
        let mut map = Map::new();
        map.insert("version".into(), Value::from(1));
        map.insert("kind".into(), Value::from("attachment-upload"));
        map.insert("type".into(), Value::from(self.r#type.as_str()));
        map.insert("attachmentId".into(), Value::from(self.attachment_id.clone()));
        map.insert("name".into(), Value::from(self.name.clone()));
        map.insert("mimeType".into(), Value::from(self.mime_type.clone()));
        map.insert("sizeBytes".into(), json_number(self.size_bytes));
        map.insert("expiresAt".into(), json_number(self.expires_at));
        Value::Object(map).to_string()
    }

    /// Decoding, with `type` defaulting to `image`.
    pub fn from_json(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        let object = value.as_object()?;
        if object.get("version")?.as_f64()? != 1.0 || object.get("kind")?.as_str()? != "attachment-upload" {
            return None;
        }
        let string = |key: &str| object.get(key).and_then(Value::as_str).map(str::to_owned);
        Some(Self {
            r#type: match object.get("type") {
                None => UploadType::Image,
                Some(Value::String(kind)) if kind == "image" => UploadType::Image,
                Some(Value::String(kind)) if kind == "file" => UploadType::File,
                Some(_) => return None,
            },
            attachment_id: string("attachmentId")?,
            name: string("name")?,
            mime_type: string("mimeType")?,
            size_bytes: object.get("sizeBytes")?.as_f64()?,
            expires_at: object.get("expiresAt")?.as_f64()?,
        })
    }
}

/// The outcome of an upload: `Ok`, or the status and text the route answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreUploadResult {
    Ok,
    Rejected { status: u16, detail: String },
}

/// Removes the `.part` file however the upload ends (including a dropped request).
struct PartFileGuard(PathBuf);

impl Drop for PartFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The upload service.
pub struct AttachmentUploads {
    attachments_dir: PathBuf,
    secrets: ServerSecretStore,
    clock: SharedClock,
    last_sweep_ms: Mutex<Option<i64>>,
}

impl std::fmt::Debug for AttachmentUploads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachmentUploads")
            .field("attachments_dir", &self.attachments_dir)
            .finish_non_exhaustive()
    }
}

impl AttachmentUploads {
    pub fn new(attachments_dir: &Path, secrets: ServerSecretStore, clock: SharedClock) -> Self {
        Self {
            attachments_dir: attachments_dir.to_owned(),
            secrets,
            clock,
            last_sweep_ms: Mutex::new(None),
        }
    }

    async fn signing_secret(&self) -> Result<Vec<u8>, Defect> {
        self.secrets
            .get_or_create_random(SIGNING_SECRET_NAME, 32)
            .await
            .map_err(|error| Defect::from_error(&error))
    }

    fn sweep_if_due(&self, now_ms: i64) {
        {
            let mut last = self.last_sweep_ms.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if last.is_some_and(|previous| now_ms - previous < PENDING_ATTACHMENT_SWEEP_INTERVAL_MS) {
                return;
            }
            *last = Some(now_ms);
        }
        let deleted = sweep_stale_pending_attachments(&self.attachments_dir, now_ms);
        if deleted > 0 {
            tracing::info!(deleted, "Removed expired attachment uploads.");
        }
    }

    /// `issueAttachmentUploadUrl` (the input already validated against its schema).
    pub async fn issue_upload_url(&self, input: &AttachmentCreateUploadUrlInput) -> Result<AttachmentCreateUploadUrlResult, AttachmentUploadSigningKeyError> {
        let secret = self.signing_secret().await.map_err(|cause| AttachmentUploadSigningKeyError {
            tag: LitAttachmentUploadSigningKeyError,
            cause: cause.0,
        })?;
        let now_ms = self.clock.now_millis();
        self.sweep_if_due(now_ms);
        let (r#type, name, mime_type, size_bytes) = match input {
            AttachmentCreateUploadUrlInput::Image(image) => (UploadType::Image, image.name.clone(), image.mime_type.as_str().to_owned(), image.size_bytes),
            AttachmentCreateUploadUrlInput::File(file) => (UploadType::File, file.name.clone(), file.mime_type.clone(), file.size_bytes),
        };
        let attachment_id = create_pending_attachment_id(
            match r#type {
                UploadType::File => Some(attachment_file_extension(&name)),
                UploadType::Image => None,
            }
            .as_deref(),
        );
        let expires_at = now_ms + ATTACHMENT_UPLOAD_URL_TTL_MS;
        let claims = AttachmentUploadClaims {
            r#type,
            attachment_id: attachment_id.clone(),
            name,
            mime_type,
            size_bytes: size_bytes as f64,
            expires_at: expires_at as f64,
        };
        let token = sign_token(&claims.to_json(), &secret);
        Ok(AttachmentCreateUploadUrlResult {
            attachment_id,
            relative_url: format!("{ATTACHMENT_UPLOAD_ROUTE_PREFIX}/{token}"),
            expires_at: JsNumber::from(expires_at),
        })
    }

    /// `validateAttachmentUploadToken`: exactly two `.` parts (an empty third is tolerated, as
    /// in TS), a valid signature, unexpired claims.
    pub async fn validate_upload_token(&self, token: &str) -> Option<AttachmentUploadClaims> {
        let mut parts = token.split('.');
        let payload = parts.next().filter(|part| !part.is_empty())?;
        let signature = parts.next().filter(|part| !part.is_empty())?;
        if parts.next().is_some_and(|extra| !extra.is_empty()) {
            return None;
        }
        let secret = match self.signing_secret().await {
            Ok(secret) => secret,
            Err(cause) => {
                tracing::error!(%cause, "Failed to load the attachment upload signing key.");
                return None;
            }
        };
        if !timing_safe_equal_base64url(signature, &sign_payload(payload, &secret)) {
            return None;
        }
        let claims = AttachmentUploadClaims::from_json(&decode_payload_text(payload)?)?;
        (claims.expires_at > self.clock.now_millis() as f64).then_some(claims)
    }

    /// `storeAttachmentUpload`: streams the body into place, refusing anything but exactly
    /// `sizeBytes` bytes.
    pub async fn store_upload<S, E>(&self, claims: &AttachmentUploadClaims, body: S) -> StoreUploadResult
    where
        S: Stream<Item = Result<bytes::Bytes, E>> + Unpin,
        E: std::fmt::Display,
    {
        let extension = match claims.r#type {
            UploadType::File => attachment_file_extension(&claims.name),
            UploadType::Image => infer_image_extension(&claims.mime_type, Some(&claims.name)),
        };
        let relative_path = format!("{}{extension}", claims.attachment_id);
        let final_path = resolve_attachment_relative_path(&self.attachments_dir, &relative_path);
        let part_path = resolve_attachment_relative_path(&self.attachments_dir, &format!("{relative_path}.{}.part", zc_core::uuid_v4()));
        let (Some(final_path), Some(part_path)) = (final_path, part_path) else {
            return StoreUploadResult::Rejected {
                status: 500,
                detail: "Failed to resolve attachment path.".into(),
            };
        };
        let _guard = PartFileGuard(part_path.clone());
        match Self::stream_to_part(&final_path, &part_path, claims.size_bytes, body).await {
            Ok(received) if received as f64 != claims.size_bytes => StoreUploadResult::Rejected {
                status: 400,
                detail: format!("Body was {received} bytes, expected {}.", JsNumber(claims.size_bytes)),
            },
            Ok(_) => match tokio::fs::rename(&part_path, &final_path).await {
                Ok(()) => StoreUploadResult::Ok,
                Err(error) => Self::persist_failure(claims, &error.to_string()),
            },
            Err(error) => Self::persist_failure(claims, &error),
        }
    }

    fn persist_failure(claims: &AttachmentUploadClaims, cause: &str) -> StoreUploadResult {
        tracing::error!(attachment_id = claims.attachment_id, cause, "Failed to persist attachment upload.");
        StoreUploadResult::Rejected {
            status: 500,
            detail: "Failed to persist upload.".into(),
        }
    }

    /// Writes chunks while the running total stays within `size_bytes` (`Stream.takeWhile`):
    /// the first chunk that overflows is counted but not written, and ends the read.
    async fn stream_to_part<S, E>(final_path: &Path, part_path: &Path, size_bytes: f64, mut body: S) -> Result<u64, String>
    where
        S: Stream<Item = Result<bytes::Bytes, E>> + Unpin,
        E: std::fmt::Display,
    {
        if let Some(parent) = final_path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|error| error.to_string())?;
        }
        let mut file = tokio::fs::File::create(part_path).await.map_err(|error| error.to_string())?;
        let mut received: u64 = 0;
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|error| error.to_string())?;
            received += chunk.len() as u64;
            if received as f64 > size_bytes {
                break;
            }
            file.write_all(&chunk).await.map_err(|error| error.to_string())?;
        }
        file.flush().await.map_err(|error| error.to_string())?;
        Ok(received)
    }

    /// `deletePendingAttachment`: only `pending-…` ids; a missing file is fine.
    pub async fn delete_pending_attachment(&self, attachment_id: &str) {
        if parse_thread_segment_from_attachment_id(attachment_id).as_deref() != Some(PENDING_ATTACHMENT_THREAD_SEGMENT) {
            return;
        }
        if let Some(path) = resolve_attachment_path_by_id(&self.attachments_dir, attachment_id) {
            let _ = tokio::fs::remove_file(path).await;
        }
    }
}

#[cfg(test)]
mod tests {
    //! `AttachmentUpload.test.ts`.
    use std::sync::Arc;

    use bytes::Bytes;
    use zc_auth::TestClock;
    use zc_contracts::{AttachmentCreateUploadUrlInputFile, AttachmentCreateUploadUrlInputImage, AttachmentCreateUploadUrlInputImageMimeType, LitFile};

    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        attachments_dir: PathBuf,
        secrets: ServerSecretStore,
        clock: Arc<TestClock>,
        uploads: AttachmentUploads,
    }

    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let attachments_dir = dir.path().join("attachments");
        std::fs::create_dir_all(&attachments_dir).unwrap();
        let secrets = ServerSecretStore::open(dir.path().join("secrets")).await.unwrap();
        let clock = TestClock::new(1_790_000_000_000);
        let uploads = AttachmentUploads::new(&attachments_dir, secrets.clone(), clock.clone());
        Fixture {
            _dir: dir,
            attachments_dir,
            secrets,
            clock,
            uploads,
        }
    }

    fn image_input() -> AttachmentCreateUploadUrlInput {
        AttachmentCreateUploadUrlInput::Image(AttachmentCreateUploadUrlInputImage {
            r#type: None,
            name: "screenshot.png".into(),
            mime_type: AttachmentCreateUploadUrlInputImageMimeType::ImagePng,
            size_bytes: 6,
        })
    }

    fn token_of(result: &AttachmentCreateUploadUrlResult) -> String {
        result
            .relative_url
            .strip_prefix(&format!("{ATTACHMENT_UPLOAD_ROUTE_PREFIX}/"))
            .unwrap()
            .to_owned()
    }

    fn chunks(parts: Vec<Vec<u8>>) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Unpin {
        futures::stream::iter(parts.into_iter().map(|part| Ok(Bytes::from(part))))
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn signs_the_metadata_and_validates_the_token() {
        let f = fixture().await;
        let issued = f.uploads.issue_upload_url(&image_input()).await.unwrap();
        assert_eq!(parse_thread_segment_from_attachment_id(&issued.attachment_id).as_deref(), Some("pending"));
        let claims = f.uploads.validate_upload_token(&token_of(&issued)).await.unwrap();
        assert_eq!(claims.attachment_id, issued.attachment_id);
        assert_eq!(
            (claims.name.as_str(), claims.mime_type.as_str(), claims.size_bytes),
            ("screenshot.png", "image/png", 6.0)
        );
        assert_eq!(claims.r#type, UploadType::Image);
    }

    #[tokio::test]
    async fn rejects_tampered_and_malformed_tokens() {
        let f = fixture().await;
        let token = token_of(&f.uploads.issue_upload_url(&image_input()).await.unwrap());
        let (payload, signature) = token.split_once('.').unwrap();
        assert!(f.uploads.validate_upload_token(&format!("{payload}x.{signature}")).await.is_none());
        assert!(f.uploads.validate_upload_token(&format!("{token}.extra")).await.is_none());
        assert!(f.uploads.validate_upload_token("garbage").await.is_none());
    }

    #[tokio::test]
    async fn accepts_unexpired_tokens_issued_before_file_support() {
        let f = fixture().await;
        let issued = f.uploads.issue_upload_url(&image_input()).await.unwrap();
        let secret = f.secrets.get_or_create_random(SIGNING_SECRET_NAME, 32).await.unwrap();
        let legacy = format!(
            r#"{{"version":1,"kind":"attachment-upload","attachmentId":"{}","name":"screenshot.png","mimeType":"image/png","sizeBytes":6,"expiresAt":{}}}"#,
            issued.attachment_id, issued.expires_at
        );
        let claims = f.uploads.validate_upload_token(&sign_token(&legacy, &secret)).await.unwrap();
        assert_eq!((claims.r#type, claims.attachment_id), (UploadType::Image, issued.attachment_id));
    }

    #[tokio::test]
    async fn rejects_expired_tokens() {
        let f = fixture().await;
        let token = token_of(&f.uploads.issue_upload_url(&image_input()).await.unwrap());
        f.clock.advance(11 * 60_000);
        assert!(f.uploads.validate_upload_token(&token).await.is_none());
    }

    #[tokio::test]
    async fn sweeps_expired_pending_uploads_when_issuing() {
        let f = fixture().await;
        let stale = f.attachments_dir.join("pending-00000000-0000-4000-8000-0000000000cc.png");
        std::fs::write(&stale, b"pixels").unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        std::fs::File::options().write(true).open(&stale).unwrap().set_modified(old).unwrap();
        f.uploads.issue_upload_url(&image_input()).await.unwrap();
        assert!(!stale.exists());
    }

    #[tokio::test]
    async fn stores_the_expected_bytes_without_temporary_files() {
        let f = fixture().await;
        let issued = f.uploads.issue_upload_url(&image_input()).await.unwrap();
        let claims = f.uploads.validate_upload_token(&token_of(&issued)).await.unwrap();
        assert_eq!(
            f.uploads.store_upload(&claims, chunks(vec![vec![1, 2, 3]])).await,
            StoreUploadResult::Rejected {
                status: 400,
                detail: "Body was 3 bytes, expected 6.".into()
            }
        );
        assert_eq!(f.uploads.store_upload(&claims, chunks(vec![vec![0; 6]])).await, StoreUploadResult::Ok);
        assert!(f.attachments_dir.join(format!("{}.png", issued.attachment_id)).exists());
        assert!(entries(&f.attachments_dir).iter().all(|name| !name.ends_with(".part")));
    }

    #[tokio::test]
    async fn streams_generic_files_with_their_extension_and_deletes_them() {
        let f = fixture().await;
        let issued = f
            .uploads
            .issue_upload_url(&AttachmentCreateUploadUrlInput::File(AttachmentCreateUploadUrlInputFile {
                r#type: LitFile,
                name: "report.PDF".into(),
                mime_type: "application/pdf".into(),
                size_bytes: 6,
            }))
            .await
            .unwrap();
        let claims = f.uploads.validate_upload_token(&token_of(&issued)).await.unwrap();
        assert_eq!(
            f.uploads.store_upload(&claims, chunks(vec![vec![1, 2, 3], vec![4, 5, 6]])).await,
            StoreUploadResult::Ok
        );
        assert!(issued.attachment_id.ends_with("-pdf"));
        assert_eq!(
            std::fs::read(f.attachments_dir.join(format!("{}.pdf", issued.attachment_id))).unwrap(),
            vec![1, 2, 3, 4, 5, 6]
        );
        f.uploads.delete_pending_attachment(&issued.attachment_id).await;
        assert!(entries(&f.attachments_dir).is_empty());
    }

    #[tokio::test]
    async fn removes_partial_uploads_that_exceed_their_size() {
        let f = fixture().await;
        let issued = f.uploads.issue_upload_url(&image_input()).await.unwrap();
        let claims = f.uploads.validate_upload_token(&token_of(&issued)).await.unwrap();
        assert!(matches!(
            f.uploads.store_upload(&claims, chunks(vec![vec![0; 7]])).await,
            StoreUploadResult::Rejected { status: 400, .. }
        ));
        assert!(entries(&f.attachments_dir).is_empty());
    }

    #[tokio::test]
    async fn removes_partial_uploads_when_interrupted() {
        let f = fixture().await;
        let issued = f.uploads.issue_upload_url(&image_input()).await.unwrap();
        let claims = f.uploads.validate_upload_token(&token_of(&issued)).await.unwrap();
        let (sender, receiver) = futures::channel::mpsc::unbounded::<Result<Bytes, std::io::Error>>();
        sender.unbounded_send(Ok(Bytes::from_static(&[1, 2, 3]))).unwrap();
        {
            let upload = f.uploads.store_upload(&claims, receiver);
            futures::pin_mut!(upload);
            // Runs until the body stalls waiting for its next chunk. The partial file is written
            // off the runtime's thread: give it up to five seconds on a slow machine.
            assert!(futures::poll!(upload.as_mut()).is_pending());
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                let _ = futures::poll!(upload.as_mut());
                if entries(&f.attachments_dir).iter().any(|name| name.ends_with(".part")) {
                    break;
                }
            }
            assert_eq!(entries(&f.attachments_dir).iter().filter(|name| name.ends_with(".part")).count(), 1);
        }
        // Dropped mid-upload.
        assert!(entries(&f.attachments_dir).is_empty());
        drop(sender);
    }

    #[tokio::test]
    async fn deletes_pending_uploads_only() {
        let f = fixture().await;
        let uuid = "00000000-0000-4000-8000-0000000000dd";
        let pending = f.attachments_dir.join(format!("pending-{uuid}.png"));
        let claimed = f.attachments_dir.join(format!("thread-1-{uuid}.png"));
        std::fs::write(&pending, b"pixels").unwrap();
        std::fs::write(&claimed, b"pixels").unwrap();
        f.uploads.delete_pending_attachment(&format!("pending-{uuid}")).await;
        f.uploads.delete_pending_attachment(&format!("pending-{uuid}")).await;
        f.uploads.delete_pending_attachment(&format!("thread-1-{uuid}")).await;
        assert!(!pending.exists());
        assert!(claimed.exists());
    }
}
