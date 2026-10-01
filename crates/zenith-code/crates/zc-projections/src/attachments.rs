//! The attachment-file side of the projections: which files under `<stateDir>/attachments`
//! belong to which thread, and the deletions a `thread.deleted` or `thread.reverted` causes.
//!
//! The naming helpers are the parts of `attachmentStore.ts`, `attachmentPaths.ts` and
//! `imageMime.ts` the pipeline needs. WP-08 (orchestration core) owns the attachment store;
//! reconcile with its copy when the crates merge.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

use crate::event::str_field;
use crate::js;

const ATTACHMENT_ID_THREAD_SEGMENT_MAX_CHARS: usize = 80;
pub const PENDING_ATTACHMENT_THREAD_SEGMENT: &str = "pending";

fn attachment_id_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new("(?i)^([a-z0-9_]+(?:-[a-z0-9_]+)*)-([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})(?:-([a-z0-9]{1,10}))?$")
            .expect("static regex")
    })
}

/// Node's `path.posix.normalize`.
pub fn posix_normalize(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let is_absolute = path.starts_with('/');
    let trailing_separator = path.ends_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if !is_absolute {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let mut out = parts.join("/");
    if out.is_empty() && !is_absolute {
        out.push('.');
    }
    if !out.is_empty() && trailing_separator {
        out.push('/');
    }
    if is_absolute {
        format!("/{out}")
    } else {
        out
    }
}

/// `normalizeAttachmentRelativePath`.
pub fn normalize_attachment_relative_path(raw: &str) -> Option<String> {
    let normalized = posix_normalize(raw);
    let normalized = normalized.trim_start_matches(['/', '\\']);
    if normalized.is_empty() || normalized.starts_with("..") || normalized.contains('\0') {
        return None;
    }
    Some(normalized.replace('\\', "/"))
}

/// `toSafeThreadAttachmentSegment`.
pub fn to_safe_thread_attachment_segment(thread_id: &str) -> Option<String> {
    static NON_SAFE: OnceLock<Regex> = OnceLock::new();
    static DASHES: OnceLock<Regex> = OnceLock::new();
    static EDGES: OnceLock<Regex> = OnceLock::new();
    static TRAILING: OnceLock<Regex> = OnceLock::new();
    let non_safe = NON_SAFE.get_or_init(|| Regex::new("(?i)[^a-z0-9_-]+").unwrap());
    let dashes = DASHES.get_or_init(|| Regex::new("-+").unwrap());
    let edges = EDGES.get_or_init(|| Regex::new("^[-_]+|[-_]+$").unwrap());
    let trailing = TRAILING.get_or_init(|| Regex::new("[-_]+$").unwrap());

    let lowered = js::trim(thread_id).to_lowercase();
    let replaced = non_safe.replace_all(&lowered, "-");
    let collapsed = dashes.replace_all(&replaced, "-");
    let trimmed = edges.replace_all(&collapsed, "");
    // Only ASCII is left, so UTF-16 and byte offsets agree.
    let sliced: String = trimmed.chars().take(ATTACHMENT_ID_THREAD_SEGMENT_MAX_CHARS).collect();
    let segment = trailing.replace_all(&sliced, "").into_owned();
    if segment.is_empty() {
        return None;
    }
    Some(if segment == PENDING_ATTACHMENT_THREAD_SEGMENT {
        "_pending".to_string()
    } else {
        segment
    })
}

/// `parseThreadSegmentFromAttachmentId`.
pub fn parse_thread_segment_from_attachment_id(attachment_id: &str) -> Option<String> {
    let normalized = normalize_attachment_relative_path(attachment_id)?;
    if normalized.contains('/') || normalized.contains('.') {
        return None;
    }
    let captures = attachment_id_pattern().captures(&normalized)?;
    Some(captures.get(1)?.as_str().to_lowercase())
}

/// `parseAttachmentIdFromRelativePath`.
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
    (!id.is_empty() && !id.contains('.')).then(|| id.to_string())
}

/// Node's `path.extname` (POSIX).
fn extname(file_name: &str) -> &str {
    let base = file_name.trim_end_matches('/');
    let base = base.rsplit('/').next().unwrap_or(base);
    match base.rfind('.') {
        Some(index) if index > 0 => &base[index..],
        _ => "",
    }
}

/// `attachmentFileExtension`.
pub fn attachment_file_extension(file_name: &str) -> String {
    static VALID: OnceLock<Regex> = OnceLock::new();
    let valid = VALID.get_or_init(|| Regex::new(r"^\.[a-z0-9]{1,10}$").unwrap());
    let extension = extname(file_name).to_lowercase();
    if extension == ".part" || !valid.is_match(&extension) {
        ".bin".to_string()
    } else {
        extension
    }
}

const SAFE_IMAGE_FILE_EXTENSIONS: &[&str] = &[
    ".avif", ".bmp", ".gif", ".heic", ".heif", ".ico", ".jpeg", ".jpg", ".png", ".svg", ".tiff", ".webp",
];

/// `inferImageExtension`. Effect's `Mime.getExtension` fallback is reduced to the registry
/// entries that land in the safe list.
pub fn infer_image_extension(mime_type: &str, file_name: Option<&str>) -> String {
    let key = mime_type.to_lowercase();
    let from_mime = match key.as_str() {
        "image/avif" => Some(".avif"),
        "image/bmp" => Some(".bmp"),
        "image/gif" => Some(".gif"),
        "image/heic" => Some(".heic"),
        "image/heif" => Some(".heif"),
        "image/jpeg" | "image/jpg" => Some(".jpg"),
        "image/png" => Some(".png"),
        "image/svg+xml" => Some(".svg"),
        "image/tiff" => Some(".tiff"),
        "image/webp" => Some(".webp"),
        // Registry (mime-db) extensions that are in the safe list.
        "image/x-icon" | "image/vnd.microsoft.icon" => Some(".ico"),
        "image/x-ms-bmp" => Some(".bmp"),
        "image/pjpeg" => Some(".jpeg"),
        _ => None,
    };
    if let Some(extension) = from_mime {
        return extension.to_string();
    }
    static NAME_EXTENSION: OnceLock<Regex> = OnceLock::new();
    let name_extension = NAME_EXTENSION.get_or_init(|| Regex::new(r"(?i)\.([a-z0-9]{1,8})$").unwrap());
    let file_name = js::trim(file_name.unwrap_or(""));
    let from_name = name_extension
        .captures(file_name)
        .and_then(|captures| captures.get(1))
        .map(|m| format!(".{}", m.as_str().to_lowercase()))
        .unwrap_or_default();
    if SAFE_IMAGE_FILE_EXTENSIONS.contains(&from_name.as_str()) {
        return from_name;
    }
    ".bin".to_string()
}

/// `attachmentRelativePath` on an encoded `ChatAttachment`.
pub fn attachment_relative_path(attachment: &Value) -> Option<String> {
    let id = str_field(attachment, "id")?;
    let name = str_field(attachment, "name");
    match str_field(attachment, "type")? {
        "image" => Some(format!("{id}{}", infer_image_extension(str_field(attachment, "mimeType").unwrap_or(""), name))),
        "file" => Some(format!("{id}{}", attachment_file_extension(name.unwrap_or("")))),
        _ => None,
    }
}

/// `collectThreadAttachmentRelativePaths`: the files the thread's messages still reference.
pub fn collect_thread_attachment_relative_paths<'a>(thread_id: &str, attachments: impl IntoIterator<Item = &'a Value>) -> HashSet<String> {
    let mut paths = HashSet::new();
    let Some(thread_segment) = to_safe_thread_attachment_segment(thread_id) else {
        return paths;
    };
    for list in attachments {
        for attachment in list.as_array().into_iter().flatten() {
            let Some(id) = str_field(attachment, "id") else {
                continue;
            };
            if parse_thread_segment_from_attachment_id(id).as_deref() != Some(&thread_segment) {
                continue;
            }
            if let Some(path) = attachment_relative_path(attachment) {
                paths.insert(path);
            }
        }
    }
    paths
}

/// The files one projected event asks to remove (`AttachmentSideEffects`). Iteration order is
/// insertion order, like the TS `Set`/`Map`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttachmentSideEffects {
    pub deleted_thread_ids: Vec<String>,
    /// Thread id → the relative paths to keep (the set is recomputed before running).
    pub pruned_thread_relative_paths: Vec<(String, HashSet<String>)>,
}

impl AttachmentSideEffects {
    pub fn is_empty(&self) -> bool {
        self.deleted_thread_ids.is_empty() && self.pruned_thread_relative_paths.is_empty()
    }

    pub fn delete_thread(&mut self, thread_id: &str) {
        if !self.deleted_thread_ids.iter().any(|id| id == thread_id) {
            self.deleted_thread_ids.push(thread_id.to_string());
        }
    }

    /// `Map.set`: replaces the value in place, or appends.
    pub fn prune_thread(&mut self, thread_id: &str, keep: HashSet<String>) {
        if let Some(entry) = self.pruned_thread_relative_paths.iter_mut().find(|(id, _)| id == thread_id) {
            entry.1 = keep;
        } else {
            self.pruned_thread_relative_paths.push((thread_id.to_string(), keep));
        }
    }
}

/// `runAttachmentSideEffects`: removes deleted threads' files, then every file of a pruned
/// thread that is not kept. Any I/O error stops it and is returned (the caller logs it).
pub fn run_attachment_side_effects(attachments_dir: &Path, side_effects: &AttachmentSideEffects) -> std::io::Result<()> {
    let read_entries = || -> std::io::Result<Vec<String>> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(attachments_dir)? {
            entries.push(entry?.file_name().to_string_lossy().into_owned());
        }
        Ok(entries)
    };
    let relative_entry = |entry: &str| -> Option<String> {
        let normalized = entry.trim_start_matches(['/', '\\']).replace('\\', "/");
        (!normalized.is_empty() && !normalized.contains('/')).then_some(normalized)
    };
    let belongs_to = |entry: &str, segment: &str| -> bool {
        parse_attachment_id_from_relative_path(entry)
            .and_then(|id| parse_thread_segment_from_attachment_id(&id))
            .is_some_and(|thread_segment| thread_segment == segment)
    };

    for thread_id in &side_effects.deleted_thread_ids {
        let Some(segment) = to_safe_thread_attachment_segment(thread_id) else {
            tracing::warn!(thread_id, "skipping attachment cleanup for unsafe thread id");
            continue;
        };
        for entry in read_entries()? {
            let Some(entry) = relative_entry(&entry) else {
                continue;
            };
            if belongs_to(&entry, &segment) {
                remove_force(&attachments_dir.join(&entry))?;
            }
        }
    }

    for (thread_id, keep) in &side_effects.pruned_thread_relative_paths {
        if side_effects.deleted_thread_ids.contains(thread_id) {
            continue;
        }
        let Some(segment) = to_safe_thread_attachment_segment(thread_id) else {
            tracing::warn!(thread_id, "skipping attachment prune for unsafe thread id");
            continue;
        };
        for entry in read_entries()? {
            let Some(entry) = relative_entry(&entry) else {
                continue;
            };
            if !belongs_to(&entry, &segment) {
                continue;
            }
            let absolute: PathBuf = attachments_dir.join(&entry);
            let metadata = std::fs::metadata(&absolute)?;
            if !metadata.is_file() {
                continue;
            }
            if !keep.contains(&entry) {
                remove_force(&absolute)?;
            }
        }
    }
    Ok(())
}

/// `fileSystem.remove(path, {force: true})`: a missing file is fine.
fn remove_force(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => match std::fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(_) => Err(error),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn names_thread_segments_like_the_store() {
        assert_eq!(to_safe_thread_attachment_segment("  Thread One!! ").as_deref(), Some("thread-one"));
        assert_eq!(to_safe_thread_attachment_segment("pending").as_deref(), Some("_pending"));
        assert_eq!(to_safe_thread_attachment_segment("---"), None);
        let long = "a".repeat(100);
        assert_eq!(to_safe_thread_attachment_segment(&long).unwrap().len(), 80);
        let id = "thread-one-0f0e0d0c-0b0a-4908-8706-050403020100";
        assert_eq!(parse_thread_segment_from_attachment_id(id).as_deref(), Some("thread-one"));
        assert_eq!(parse_thread_segment_from_attachment_id(&format!("{id}-png")).as_deref(), Some("thread-one"));
        assert_eq!(parse_thread_segment_from_attachment_id("x.png"), None);
        assert_eq!(parse_attachment_id_from_relative_path(&format!("{id}.png")).as_deref(), Some(id));
        assert_eq!(parse_attachment_id_from_relative_path(".png"), None);
        assert_eq!(parse_attachment_id_from_relative_path("a/b.png"), None);
    }

    #[test]
    fn derives_relative_paths() {
        let image = json!({"type": "image", "id": "t-1", "name": "shot.PNG", "mimeType": "image/png", "sizeBytes": 1});
        assert_eq!(attachment_relative_path(&image).as_deref(), Some("t-1.png"));
        let odd = json!({"type": "image", "id": "t-2", "name": "shot.WebP", "mimeType": "application/octet-stream", "sizeBytes": 1});
        assert_eq!(attachment_relative_path(&odd).as_deref(), Some("t-2.webp"));
        let file = json!({"type": "file", "id": "t-3", "name": "notes.TXT", "mimeType": "text/plain", "sizeBytes": 1});
        assert_eq!(attachment_relative_path(&file).as_deref(), Some("t-3.txt"));
        let part = json!({"type": "file", "id": "t-4", "name": "upload.part", "mimeType": "", "sizeBytes": 1});
        assert_eq!(attachment_relative_path(&part).as_deref(), Some("t-4.bin"));
        let unknown = json!({"type": "audio", "id": "t-5", "name": "a", "mimeType": "", "sizeBytes": 1});
        assert_eq!(attachment_relative_path(&unknown), None);
        assert_eq!(posix_normalize("a/./b/../c"), "a/c");
        assert_eq!(posix_normalize("../a"), "../a");
        assert_eq!(normalize_attachment_relative_path("/x"), Some("x".into()));
        assert_eq!(normalize_attachment_relative_path("../x"), None);
    }
}
