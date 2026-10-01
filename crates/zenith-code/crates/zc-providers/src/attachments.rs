//! Where chat attachments live on disk, as the provider service references them in prompts.
//! Port of the read-only half of `attachmentStore.ts` / `attachmentPaths.ts` / `imageMime.ts`
//! and of `provider/userInputAttachments.ts`. Uploading, claiming and sweeping attachments is
//! the orchestration's (WP-08).

use std::path::{Component, Path, PathBuf};

use zc_contracts::{ChatAttachment, ChatImageAttachmentOrChatFileAttachment, ProviderUserInputAnswers, UserInputAttachments};

use crate::errors::ProviderServiceError;
use crate::js_json;

const ATTACHMENT_ID_THREAD_SEGMENT_MAX_CHARS: usize = 80;
const PENDING_ATTACHMENT_THREAD_SEGMENT: &str = "pending";

/// `String.prototype.trim`: Unicode white space plus the BOM.
pub fn js_trim(value: &str) -> &str {
    value.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// `toSafeThreadAttachmentSegment`: the file-name-safe form of a thread id (`None` when nothing
/// usable is left).
pub fn to_safe_thread_attachment_segment(thread_id: &str) -> Option<String> {
    let lowered = js_trim(thread_id).to_lowercase();
    let mut replaced = String::with_capacity(lowered.len());
    let mut in_run = false;
    for character in lowered.chars() {
        let allowed = character.is_ascii_alphanumeric() || character == '_' || character == '-';
        if allowed {
            in_run = false;
            replaced.push(character);
        } else if !in_run {
            in_run = true;
            replaced.push('-');
        }
    }
    let mut collapsed = String::with_capacity(replaced.len());
    for character in replaced.chars() {
        if character == '-' && collapsed.ends_with('-') {
            continue;
        }
        collapsed.push(character);
    }
    let trimmed = collapsed.trim_matches(|c| c == '-' || c == '_');
    // Only ASCII is left, so byte slicing is UTF-16 slicing.
    let sliced = &trimmed[..trimmed.len().min(ATTACHMENT_ID_THREAD_SEGMENT_MAX_CHARS)];
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

/// `SAFE_IMAGE_FILE_EXTENSIONS`.
pub const SAFE_IMAGE_FILE_EXTENSIONS: &[&str] = &[
    ".avif", ".bmp", ".gif", ".heic", ".heif", ".ico", ".jpeg", ".jpg", ".png", ".svg", ".tiff", ".webp",
];

/// The part of Effect's MIME registry (`Mime.getExtension`) that can yield a safe image
/// extension the table above does not already cover.
const REGISTRY_IMAGE_EXTENSIONS: &[(&str, &str)] = &[("image/x-icon", ".ico"), ("image/vnd.microsoft.icon", ".ico"), ("image/pjpeg", ".jpeg")];

/// `inferImageExtension({mimeType, fileName})`.
pub fn infer_image_extension(mime_type: &str, file_name: Option<&str>) -> String {
    let key = mime_type.to_lowercase();
    if let Some((_, extension)) = IMAGE_EXTENSION_BY_MIME_TYPE.iter().find(|(mime, _)| *mime == key) {
        return (*extension).to_owned();
    }
    if let Some((_, extension)) = REGISTRY_IMAGE_EXTENSIONS.iter().find(|(mime, _)| *mime == key) {
        return (*extension).to_owned();
    }
    let file_name = file_name.map(js_trim).unwrap_or("");
    if let Some(dot) = file_name.rfind('.') {
        let extension = &file_name[dot + 1..];
        if (1..=8).contains(&extension.len()) && extension.chars().all(|c| c.is_ascii_alphanumeric()) {
            let dotted = format!(".{}", extension.to_ascii_lowercase());
            if SAFE_IMAGE_FILE_EXTENSIONS.contains(&dotted.as_str()) {
                return dotted;
            }
        }
    }
    ".bin".to_owned()
}

/// Node `path.extname` of a file name.
fn extname(file_name: &str) -> &str {
    let base = file_name.rsplit('/').next().unwrap_or(file_name);
    match base.rfind('.') {
        Some(0) | None => "",
        Some(index) => &base[index..],
    }
}

/// `attachmentFileExtension(fileName)`.
pub fn attachment_file_extension(file_name: &str) -> String {
    let extension = extname(file_name).to_lowercase();
    let valid = extension.len() >= 2 && extension.len() <= 11 && extension[1..].chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if extension == ".part" || !valid {
        return ".bin".to_owned();
    }
    extension
}

/// `attachmentRelativePath(attachment)`: `None` for attachment types this build does not know.
pub fn attachment_relative_path(attachment: &ChatAttachment) -> Option<String> {
    match attachment {
        ChatAttachment::ChatImageAttachment(image) => Some(format!("{}{}", image.id, infer_image_extension(&image.mime_type, Some(&image.name)))),
        ChatAttachment::ChatFileAttachment(file) => Some(format!("{}{}", file.id, attachment_file_extension(&file.name))),
        ChatAttachment::ChatUnknownAttachment(_) => None,
    }
}

/// `normalizeAttachmentRelativePath`: Node `path.normalize`, leading separators dropped, no
/// `..` escape, no NUL.
pub fn normalize_attachment_relative_path(raw: &str) -> Option<String> {
    if raw.contains('\0') {
        return None;
    }
    let normalized = zc_core::paths::normalize_lexically(Path::new(&raw.replace('\\', "/")));
    let text = normalized.to_string_lossy().trim_start_matches('/').to_owned();
    if text.is_empty() || text == "." || text.starts_with("..") {
        return None;
    }
    Some(text)
}

/// `resolveAttachmentRelativePath`: an absolute path strictly inside `attachments_dir`.
pub fn resolve_attachment_relative_path(attachments_dir: &Path, relative_path: &str) -> Option<PathBuf> {
    let normalized = normalize_attachment_relative_path(relative_path)?;
    let root = zc_core::paths::resolve_path(attachments_dir);
    let file_path = zc_core::paths::normalize_lexically(&root.join(&normalized));
    let inside = file_path.starts_with(&root) && file_path != root && !file_path.components().any(|c| matches!(c, Component::ParentDir));
    inside.then_some(file_path)
}

/// `resolveAttachmentPath({attachmentsDir, attachment})`.
pub fn resolve_attachment_path(attachments_dir: &Path, attachment: &ChatAttachment) -> Option<PathBuf> {
    resolve_attachment_relative_path(attachments_dir, &attachment_relative_path(attachment)?)
}

/// `appendUserInputAttachmentPaths`: answers to a structured question get one line per attached
/// file (`Attached image "name": "/path"`), after the typed answer. Provider answer protocols
/// stay unchanged.
pub fn append_user_input_attachment_paths(
    answers: &ProviderUserInputAnswers,
    attachments_by_question_id: Option<&UserInputAttachments>,
    attachments_dir: &Path,
) -> Result<ProviderUserInputAnswers, ProviderServiceError> {
    let mut answers = answers.clone();
    let Some(attachments_by_question_id) = attachments_by_question_id else {
        return Ok(answers);
    };
    for (question_id, attachments) in attachments_by_question_id.iter() {
        if attachments.is_empty() {
            continue;
        }
        let mut references = Vec::new();
        for attachment in attachments {
            let attachment = &match attachment {
                ChatImageAttachmentOrChatFileAttachment::ChatImageAttachment(image) => ChatAttachment::ChatImageAttachment(image.clone()),
                ChatImageAttachmentOrChatFileAttachment::ChatFileAttachment(file) => ChatAttachment::ChatFileAttachment(file.clone()),
            };
            let (kind, name) = attachment_kind_and_name(attachment);
            let path = resolve_attachment_path(attachments_dir, attachment);
            let exists = match &path {
                Some(path) => match std::fs::metadata(path) {
                    Ok(_) => true,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                    Err(_) => {
                        return Err(ProviderServiceError::validation(
                            "respondToUserInput",
                            format!("Could not access attachment '{name}'."),
                        ))
                    }
                },
                None => false,
            };
            let Some(path) = path.filter(|_| exists) else {
                return Err(ProviderServiceError::validation(
                    "respondToUserInput",
                    format!("Attachment '{name}' is no longer available. Attach it again."),
                ));
            };
            let mut quoted_name = String::new();
            js_json::write_string(&mut quoted_name, name);
            let mut quoted_path = String::new();
            js_json::write_string(&mut quoted_path, &path.to_string_lossy());
            references.push(format!("Attached {kind} {quoted_name}: {quoted_path}"));
        }
        let text = references.join("\n");
        let next = match answers.get(question_id) {
            Some(serde_json::Value::Array(items)) => {
                let mut items = items.clone();
                items.push(serde_json::Value::String(text));
                serde_json::Value::Array(items)
            }
            Some(serde_json::Value::String(answer)) if !answer.is_empty() => serde_json::Value::String(format!("{answer}\n\n{text}")),
            _ => serde_json::Value::String(text),
        };
        answers.insert(question_id.clone(), next);
    }
    Ok(answers)
}

/// `(type, name)` of an attachment, whatever its variant.
pub fn attachment_kind_and_name(attachment: &ChatAttachment) -> (&str, &str) {
    match attachment {
        ChatAttachment::ChatImageAttachment(image) => ("image", &image.name),
        ChatAttachment::ChatFileAttachment(file) => ("file", &file.name),
        ChatAttachment::ChatUnknownAttachment(other) => (&other.r#type, &other.name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_segments_match_the_attachment_store() {
        assert_eq!(to_safe_thread_attachment_segment("Thread 1/Alpha").as_deref(), Some("thread-1-alpha"));
        assert_eq!(to_safe_thread_attachment_segment("  --x__ ").as_deref(), Some("x"));
        assert_eq!(to_safe_thread_attachment_segment("pending").as_deref(), Some("_pending"));
        assert_eq!(to_safe_thread_attachment_segment("///"), None);
        let long = "a".repeat(79) + "-b";
        assert_eq!(to_safe_thread_attachment_segment(&long).as_deref(), Some("a".repeat(79).as_str()));
    }

    #[test]
    fn image_and_file_extensions() {
        assert_eq!(infer_image_extension("image/PNG", None), ".png");
        assert_eq!(infer_image_extension("application/octet-stream", Some("shot.JPEG")), ".jpeg");
        assert_eq!(infer_image_extension("application/octet-stream", Some("notes.txt")), ".bin");
        assert_eq!(attachment_file_extension("Report.PDF"), ".pdf");
        assert_eq!(attachment_file_extension("archive.part"), ".bin");
        assert_eq!(attachment_file_extension("noext"), ".bin");
    }

    #[test]
    fn relative_paths_stay_inside_the_root() {
        let root = Path::new("/tmp/attachments");
        assert_eq!(
            resolve_attachment_relative_path(root, "a/b.png"),
            Some(PathBuf::from("/tmp/attachments/a/b.png"))
        );
        assert_eq!(resolve_attachment_relative_path(root, "../escape.png"), None);
        assert_eq!(
            resolve_attachment_relative_path(root, "/abs.png"),
            Some(PathBuf::from("/tmp/attachments/abs.png"))
        );
        assert_eq!(resolve_attachment_relative_path(root, ""), None);
    }
}
