//! Chat attachment files under `<stateDir>/attachments` (`attachmentStore.ts`,
//! `attachmentPaths.ts`, and the `imageMime.ts` helpers they use).
//!
//! An attachment id is `<threadSegment>-<uuid>[-<ext>]`; its file is the id plus an
//! extension. Uploads land as `pending-<uuid>[-<ext>]` and are copied to a thread-named id when
//! a turn claims them. Partial uploads end in `.part`. [`sweep_stale_pending_attachments`]
//! (run at startup and by the upload route) deletes pending uploads older than a day and
//! partial ones older than an hour.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use zc_contracts::ChatAttachment;
use zc_core::ids::uuid_v4;

pub const PENDING_ATTACHMENT_THREAD_SEGMENT: &str = "pending";
pub const PENDING_ATTACHMENT_MAX_AGE_MS: i64 = 24 * 60 * 60 * 1000;
pub const PARTIAL_UPLOAD_MAX_AGE_MS: i64 = 60 * 60 * 1000;
const ATTACHMENT_ID_THREAD_SEGMENT_MAX_CHARS: usize = 80;

/// `SAFE_IMAGE_FILE_EXTENSIONS` (`imageMime.ts`).
pub const SAFE_IMAGE_FILE_EXTENSIONS: &[&str] = &[
    ".avif", ".bmp", ".gif", ".heic", ".heif", ".ico", ".jpeg", ".jpg", ".png", ".svg", ".tiff", ".webp",
];

/// `IMAGE_EXTENSION_BY_MIME_TYPE` (`imageMime.ts`).
const IMAGE_EXTENSION_BY_MIME_TYPE: &[(&str, &str)] = &[
    ("image/avif", ".avif"),
    ("image/bmp", ".bmp"),
    ("image/gif", ".gif"),
    ("image/heic", ".heic"),
    ("image/heif", ".heif"),
    ("image/jpeg", ".jpg"),
    ("image/jpg", ".jpg"),
    ("image/png", ".png"),
    ("image/svg+xml", ".svg"),
    ("image/tiff", ".tiff"),
    ("image/webp", ".webp"),
];

/// The standard image types whose default extension (Effect's `Mime.getExtension`) is a safe
/// one: every other standard type maps to an unsafe extension or none.
const STANDARD_IMAGE_EXTENSIONS: &[(&str, &str)] = &[
    ("image/avif", ".avif"),
    ("image/bmp", ".bmp"),
    ("image/gif", ".gif"),
    ("image/heic", ".heic"),
    ("image/heif", ".heif"),
    ("image/jpeg", ".jpg"),
    ("image/png", ".png"),
    ("image/svg+xml", ".svg"),
    ("image/webp", ".webp"),
];

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static regex"))
}

fn attachment_id_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    regex(
        &PATTERN,
        r"(?i)^([a-z0-9_]+(?:-[a-z0-9_]+)*)-([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})(?:-([a-z0-9]{1,10}))?$",
    )
}

// ---------------------------------------------------------------------------------------------
// Paths (`attachmentPaths.ts`)
// ---------------------------------------------------------------------------------------------

/// Node's posix `path.normalize`.
pub fn posix_normalize(path: &str) -> String {
    if path.is_empty() {
        return ".".to_owned();
    }
    let absolute = path.starts_with('/');
    let trailing = path.ends_with('/');
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.last().is_some_and(|last| *last != "..") {
                    segments.pop();
                } else if !absolute {
                    segments.push("..");
                }
            }
            other => segments.push(other),
        }
    }
    let mut out = segments.join("/");
    if out.is_empty() {
        if absolute {
            return "/".to_owned();
        }
        return if trailing { "./".to_owned() } else { ".".to_owned() };
    }
    if trailing {
        out.push('/');
    }
    if absolute {
        format!("/{out}")
    } else {
        out
    }
}

/// Node's posix `path.extname`.
pub fn extname(file_name: &str) -> &str {
    let base = file_name.rsplit('/').next().unwrap_or(file_name);
    let base = if base.is_empty() {
        // `extname("a/")` looks at the last non-empty segment.
        file_name.trim_end_matches('/').rsplit('/').next().unwrap_or("")
    } else {
        base
    };
    match base.rfind('.') {
        Some(0) | None => "",
        Some(index) => &base[index..],
    }
}

/// `normalizeAttachmentRelativePath`: `None` for an empty path, one that climbs out, or one with
/// a NUL.
pub fn normalize_attachment_relative_path(raw: &str) -> Option<String> {
    let normalized = posix_normalize(raw);
    let normalized = normalized.trim_start_matches(['/', '\\']);
    if normalized.is_empty() || normalized.starts_with("..") || normalized.contains('\0') {
        return None;
    }
    Some(normalized.replace('\\', "/"))
}

/// `resolveAttachmentRelativePath`: the absolute path inside `attachments_dir`, or `None` when
/// the relative path would leave it.
pub fn resolve_attachment_relative_path(attachments_dir: &Path, relative_path: &str) -> Option<PathBuf> {
    let normalized = normalize_attachment_relative_path(relative_path)?;
    let root = zc_core::paths::resolve_path(attachments_dir);
    let root = root.to_string_lossy();
    let file_path = posix_normalize(&format!("{root}/{normalized}"));
    if !file_path.starts_with(&format!("{root}/")) {
        return None;
    }
    Some(PathBuf::from(file_path))
}

// ---------------------------------------------------------------------------------------------
// Ids and files (`attachmentStore.ts`)
// ---------------------------------------------------------------------------------------------

/// `toSafeThreadAttachmentSegment`: the thread id as an id segment (`pending` is reserved).
pub fn to_safe_thread_attachment_segment(thread_id: &str) -> Option<String> {
    static INVALID: OnceLock<Regex> = OnceLock::new();
    static DASHES: OnceLock<Regex> = OnceLock::new();
    let lowered = crate::support::js_trim(thread_id).to_lowercase();
    let replaced = regex(&INVALID, r"(?i)[^a-z0-9_-]+").replace_all(&lowered, "-");
    let collapsed = regex(&DASHES, r"-+").replace_all(&replaced, "-");
    let trimmed = collapsed.trim_matches(['-', '_']);
    let sliced: String = trimmed.chars().take(ATTACHMENT_ID_THREAD_SEGMENT_MAX_CHARS).collect();
    let segment = sliced.trim_end_matches(['-', '_']);
    if segment.is_empty() {
        return None;
    }
    Some(if segment == PENDING_ATTACHMENT_THREAD_SEGMENT {
        "_pending".to_owned()
    } else {
        segment.to_owned()
    })
}

/// `attachmentFileExtension`: the stored extension of a file attachment. `.part` is reserved
/// for in-flight uploads (a stored `archive.part` would look stale to the sweep).
pub fn attachment_file_extension(file_name: &str) -> String {
    static VALID: OnceLock<Regex> = OnceLock::new();
    let extension = extname(file_name).to_lowercase();
    if extension == ".part" || !regex(&VALID, r"^\.[a-z0-9]{1,10}$").is_match(&extension) {
        return ".bin".to_owned();
    }
    extension
}

fn attachment_id_extension_suffix(extension: Option<&str>) -> String {
    static VALID: OnceLock<Regex> = OnceLock::new();
    let Some(extension) = extension.filter(|extension| !extension.is_empty()) else {
        return String::new();
    };
    let normalized = extension.strip_prefix('.').unwrap_or(extension).to_lowercase();
    if regex(&VALID, r"^[a-z0-9]{1,10}$").is_match(&normalized) {
        format!("-{normalized}")
    } else {
        "-bin".to_owned()
    }
}

/// `createPendingAttachmentId`.
pub fn create_pending_attachment_id(extension: Option<&str>) -> String {
    format!("{PENDING_ATTACHMENT_THREAD_SEGMENT}-{}{}", uuid_v4(), attachment_id_extension_suffix(extension))
}

/// `createAttachmentId`.
pub fn create_attachment_id(thread_id: &str, extension: Option<&str>) -> Option<String> {
    let segment = to_safe_thread_attachment_segment(thread_id)?;
    Some(format!("{segment}-{}{}", uuid_v4(), attachment_id_extension_suffix(extension)))
}

fn parse_part(attachment_id: &str, group: usize) -> Option<String> {
    let normalized = normalize_attachment_relative_path(attachment_id)?;
    if normalized.contains('/') || normalized.contains('.') {
        return None;
    }
    attachment_id_pattern()
        .captures(&normalized)
        .and_then(|captures| captures.get(group).map(|part| part.as_str().to_lowercase()))
}

/// `parseAttachmentUuid`.
pub fn parse_attachment_uuid(attachment_id: &str) -> Option<String> {
    parse_part(attachment_id, 2)
}

/// `parseAttachmentFileExtension`.
pub fn parse_attachment_file_extension(attachment_id: &str) -> Option<String> {
    parse_part(attachment_id, 3)
}

/// `parseThreadSegmentFromAttachmentId`.
pub fn parse_thread_segment_from_attachment_id(attachment_id: &str) -> Option<String> {
    parse_part(attachment_id, 1)
}

/// `inferImageExtension` (`imageMime.ts`).
pub fn infer_image_extension(mime_type: &str, file_name: Option<&str>) -> String {
    let key = mime_type.to_lowercase();
    if let Some((_, extension)) = IMAGE_EXTENSION_BY_MIME_TYPE.iter().find(|(mime, _)| *mime == key) {
        return (*extension).to_owned();
    }
    let essence = crate::support::js_trim(mime_type.split(';').next().unwrap_or("")).to_lowercase();
    if let Some((_, extension)) = STANDARD_IMAGE_EXTENSIONS.iter().find(|(mime, _)| *mime == essence) {
        return (*extension).to_owned();
    }
    static FILE_EXTENSION: OnceLock<Regex> = OnceLock::new();
    let file_name = file_name.map(crate::support::js_trim).unwrap_or("");
    let from_name = regex(&FILE_EXTENSION, r"(?i)\.([a-z0-9]{1,8})$")
        .captures(file_name)
        .and_then(|captures| captures.get(1))
        .map(|extension| format!(".{}", extension.as_str().to_lowercase()))
        .unwrap_or_default();
    if SAFE_IMAGE_FILE_EXTENSIONS.contains(&from_name.as_str()) {
        return from_name;
    }
    ".bin".to_owned()
}

/// `attachmentRelativePath`: `None` for attachment types this build does not know.
pub fn attachment_relative_path(attachment: &ChatAttachment) -> Option<String> {
    match attachment {
        ChatAttachment::ChatImageAttachment(image) => Some(format!("{}{}", image.id, infer_image_extension(&image.mime_type, Some(&image.name)))),
        ChatAttachment::ChatFileAttachment(file) => Some(format!("{}{}", file.id, attachment_file_extension(&file.name))),
        ChatAttachment::ChatUnknownAttachment(_) => None,
    }
}

/// `resolveAttachmentPath`.
pub fn resolve_attachment_path(attachments_dir: &Path, attachment: &ChatAttachment) -> Option<PathBuf> {
    resolve_attachment_relative_path(attachments_dir, &attachment_relative_path(attachment)?)
}

/// `resolveAttachmentPathById`: the existing file of an id, trying the safe extensions when
/// the id does not carry one.
pub fn resolve_attachment_path_by_id(attachments_dir: &Path, attachment_id: &str) -> Option<PathBuf> {
    let normalized = normalize_attachment_relative_path(attachment_id)?;
    if normalized.contains('/') || normalized.contains('.') {
        return None;
    }
    if let Some(extension) = parse_attachment_file_extension(&normalized) {
        let path = resolve_attachment_relative_path(attachments_dir, &format!("{normalized}.{extension}"))?;
        return path.exists().then_some(path);
    }
    SAFE_IMAGE_FILE_EXTENSIONS
        .iter()
        .copied()
        .chain([".bin"])
        .filter_map(|extension| resolve_attachment_relative_path(attachments_dir, &format!("{normalized}{extension}")))
        .find(|path| path.exists())
}

/// `AttachmentClaimPlan`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentClaimPlan {
    Ok {
        final_id: String,
        current_path: PathBuf,
        final_path: PathBuf,
    },
    Rejected {
        reason: String,
    },
}

/// `planAttachmentClaim`: where a pending upload goes when `thread_id` claims it.
pub fn plan_attachment_claim(attachments_dir: &Path, thread_id: &str, attachment_id: &str) -> AttachmentClaimPlan {
    let rejected = |reason: &str| AttachmentClaimPlan::Rejected { reason: reason.to_owned() };
    let uuid = parse_attachment_uuid(attachment_id);
    let requested_segment = parse_thread_segment_from_attachment_id(attachment_id);
    let (Some(_), Some(requested_segment)) = (uuid, requested_segment) else {
        return rejected("invalid attachment id");
    };
    if to_safe_thread_attachment_segment(thread_id).is_none() {
        return rejected("invalid thread id");
    }
    if requested_segment != PENDING_ATTACHMENT_THREAD_SEGMENT {
        return rejected("attachment must be a pending upload");
    }
    let Some(current_path) = resolve_attachment_path_by_id(attachments_dir, attachment_id) else {
        return rejected("attachment not found (removed or expired)");
    };
    let file_extension = parse_attachment_file_extension(attachment_id);
    let Some(final_id) = create_attachment_id(thread_id, file_extension.as_deref()) else {
        return rejected("failed to create attachment id");
    };
    let current_extension = extname(&current_path.to_string_lossy()).to_owned();
    let Some(final_path) = resolve_attachment_relative_path(attachments_dir, &format!("{final_id}{current_extension}")) else {
        return rejected("failed to resolve attachment path");
    };
    AttachmentClaimPlan::Ok {
        final_id,
        current_path,
        final_path,
    }
}

/// `parseAttachmentIdFromRelativePath`: the id of a file name in the attachments directory.
pub fn parse_attachment_id_from_relative_path(relative_path: &str) -> Option<String> {
    let normalized = normalize_attachment_relative_path(relative_path)?;
    if normalized.contains('/') {
        return None;
    }
    let extension_index = normalized.rfind('.')?;
    if extension_index == 0 {
        return None;
    }
    let id = &normalized[..extension_index];
    (!id.is_empty() && !id.contains('.')).then(|| id.to_owned())
}

/// `sweepStalePendingAttachments`: deletes pending uploads older than a day and partial uploads
/// older than an hour; returns how many files went. A missing directory sweeps nothing.
pub fn sweep_stale_pending_attachments(attachments_dir: &Path, now_ms: i64) -> usize {
    let Ok(entries) = std::fs::read_dir(attachments_dir) else { return 0 };
    let mut deleted = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_partial = name.ends_with(".part");
        if !is_partial {
            let pending = parse_attachment_id_from_relative_path(&name)
                .and_then(|id| parse_thread_segment_from_attachment_id(&id))
                .is_some_and(|segment| segment == PENDING_ATTACHMENT_THREAD_SEGMENT);
            if !pending {
                continue;
            }
        }
        let Some(resolved) = resolve_attachment_relative_path(attachments_dir, &name) else {
            continue;
        };
        let Ok(metadata) = std::fs::metadata(&resolved) else { continue };
        let Ok(modified) = metadata.modified() else { continue };
        let mtime_ms = match modified.duration_since(std::time::UNIX_EPOCH) {
            Ok(elapsed) => elapsed.as_millis() as i64,
            Err(before) => -(before.duration().as_millis() as i64),
        };
        let max_age = if is_partial {
            PARTIAL_UPLOAD_MAX_AGE_MS
        } else {
            PENDING_ATTACHMENT_MAX_AGE_MS
        };
        if now_ms - mtime_ms > max_age && std::fs::remove_file(&resolved).is_ok() {
            deleted += 1;
        }
    }
    deleted
}

/// The startup sweep (`ensureServerDirectories` in `config.ts`): run it right after
/// `zc_core::config::ensure_server_directories`. Returns how many files went.
pub fn sweep_at_startup(attachments_dir: &Path) -> usize {
    let deleted = sweep_stale_pending_attachments(attachments_dir, zc_core::time::now_millis());
    if deleted > 0 {
        tracing::info!(deleted, "Removed expired attachment uploads.");
    }
    deleted
}

// ---------------------------------------------------------------------------------------------
// Data URLs (`imageMime.ts` `parseBase64DataUrl`)
// ---------------------------------------------------------------------------------------------

/// A parsed base64 data URL: the lowercased mime type and the base64 payload without white
/// space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataUrl {
    pub mime_type: String,
    pub base64: String,
}

/// `parseBase64DataUrl`: `data:<mime>[;…];base64,<payload>`, white space allowed inside the
/// payload, `=` only as one or two trailing pads.
pub fn parse_base64_data_url(data_url: &str) -> Option<DataUrl> {
    let trimmed = crate::support::js_trim(data_url);
    if trimmed.get(..5).map(str::to_ascii_lowercase).as_deref() != Some("data:") {
        return None;
    }
    let comma = trimmed.find(',')?;
    let header = &trimmed[5..comma];
    if header.is_empty() {
        return None;
    }
    let parts: Vec<&str> = header.split(';').map(crate::support::js_trim).filter(|part| !part.is_empty()).collect();
    if parts.len() < 2 || parts.last()?.to_lowercase() != "base64" {
        return None;
    }
    let mime_type = parts[0].to_lowercase();
    if mime_type.is_empty() {
        return None;
    }
    let payload = &trimmed[comma + 1..];
    let mut base64 = String::with_capacity(payload.len());
    for c in payload.chars() {
        match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '+' | '/' | '=' => base64.push(c),
            '\r' | '\n' | ' ' => {}
            _ => return None,
        }
    }
    if base64.is_empty() || !base64.len().is_multiple_of(4) {
        return None;
    }
    if let Some(first_pad) = base64.find('=') {
        if base64.len() - first_pad > 2 || base64[first_pad..].chars().any(|c| c != '=') {
            return None;
        }
    }
    Some(DataUrl { mime_type, base64 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_like_node() {
        assert_eq!(posix_normalize("a/../../b"), "../b");
        assert_eq!(posix_normalize("/a//b/./c/"), "/a/b/c/");
        assert_eq!(posix_normalize(""), ".");
        assert_eq!(posix_normalize("/.."), "/");
        assert_eq!(extname("archive.tar.gz"), ".gz");
        assert_eq!(extname(".bashrc"), "");
        assert_eq!(extname("name."), ".");
        assert_eq!(extname("noext"), "");
        assert_eq!(normalize_attachment_relative_path("../x"), None);
        assert_eq!(normalize_attachment_relative_path("/abs/x"), Some("abs/x".into()));
        assert_eq!(normalize_attachment_relative_path("a\\b"), Some("a/b".into()));
    }

    #[test]
    fn resolves_inside_the_attachments_dir_only() {
        let dir = Path::new("/tmp/zc-attachments");
        assert_eq!(resolve_attachment_relative_path(dir, "x.png"), Some(PathBuf::from("/tmp/zc-attachments/x.png")));
        assert_eq!(resolve_attachment_relative_path(dir, "a/../../x"), None);
        assert_eq!(resolve_attachment_relative_path(dir, "."), None);
    }

    #[test]
    fn makes_safe_thread_segments() {
        assert_eq!(to_safe_thread_attachment_segment(" Thread #1 "), Some("thread-1".into()));
        assert_eq!(to_safe_thread_attachment_segment("pending"), Some("_pending".into()));
        assert_eq!(to_safe_thread_attachment_segment("---"), None);
        assert_eq!(to_safe_thread_attachment_segment(&"a".repeat(100)).unwrap().len(), 80);
    }

    #[test]
    fn parses_attachment_ids() {
        let id = "thread-1-12345678-1234-1234-1234-123456789abc-png";
        assert_eq!(parse_thread_segment_from_attachment_id(id), Some("thread-1".into()));
        assert_eq!(parse_attachment_uuid(id), Some("12345678-1234-1234-1234-123456789abc".into()));
        assert_eq!(parse_attachment_file_extension(id), Some("png".into()));
        assert_eq!(parse_attachment_uuid("nope"), None);
        assert_eq!(parse_attachment_id_from_relative_path("abc.png"), Some("abc".into()));
        assert_eq!(parse_attachment_id_from_relative_path(".png"), None);
        assert_eq!(attachment_file_extension("notes.PART"), ".bin");
        assert_eq!(attachment_file_extension("notes.md"), ".md");
        assert_eq!(attachment_file_extension("noext"), ".bin");
        let pending = create_pending_attachment_id(Some(".PDF"));
        assert!(pending.starts_with("pending-") && pending.ends_with("-pdf"));
    }

    #[test]
    fn infers_image_extensions() {
        assert_eq!(infer_image_extension("image/png", None), ".png");
        assert_eq!(infer_image_extension("IMAGE/JPEG", None), ".jpg");
        assert_eq!(infer_image_extension("image/png; charset=binary", None), ".png");
        assert_eq!(infer_image_extension("application/octet-stream", Some("pic.ICO")), ".ico");
        assert_eq!(infer_image_extension("application/octet-stream", Some("pic.exe")), ".bin");
    }

    #[test]
    fn parses_data_urls() {
        assert_eq!(
            parse_base64_data_url("data:image/PNG;base64,aGVs bG8=\n"),
            Some(DataUrl {
                mime_type: "image/png".into(),
                base64: "aGVsbG8=".into()
            })
        );
        assert_eq!(parse_base64_data_url("data:image/png,abcd"), None);
        assert_eq!(parse_base64_data_url("data:image/png;base64,ab=c"), None);
        assert_eq!(parse_base64_data_url("data:image/png;base64,abc"), None);
        assert_eq!(parse_base64_data_url("data:image/png;base64,ab!d"), None);
    }
}
