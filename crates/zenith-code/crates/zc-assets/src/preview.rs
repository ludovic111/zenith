//! What the asset routes may serve, by extension (`@t3tools/shared/filePreview`,
//! `@t3tools/shared/video`, `@t3tools/shared/projectFavicon`), and the Node `path` helpers the
//! asset code relies on.

/// `PROJECT_FAVICON_FALLBACK_MARKER`: the name of a signed favicon URL that resolves to nothing,
/// so the client draws its fallback icon.
pub const PROJECT_FAVICON_FALLBACK_MARKER: &str = "project-favicon-missing";

/// `WORKSPACE_BROWSER_PREVIEW_EXTENSIONS`.
pub const WORKSPACE_BROWSER_PREVIEW_EXTENSIONS: &[&str] = &[".htm", ".html", ".pdf"];

/// `WORKSPACE_IMAGE_PREVIEW_EXTENSIONS`.
pub const WORKSPACE_IMAGE_PREVIEW_EXTENSIONS: &[&str] = &[".avif", ".gif", ".ico", ".jpeg", ".jpg", ".png", ".svg", ".webp"];

const IMAGE_MIME_TYPE_BY_EXTENSION: &[(&str, &str)] = &[
    (".avif", "image/avif"),
    (".gif", "image/gif"),
    (".ico", "image/x-icon"),
    (".jpeg", "image/jpeg"),
    (".jpg", "image/jpeg"),
    (".png", "image/png"),
    (".svg", "image/svg+xml"),
    (".webp", "image/webp"),
];

const BROWSER_MIME_TYPE_BY_EXTENSION: &[(&str, &str)] = &[(".htm", "text/html"), (".html", "text/html"), (".pdf", "application/pdf")];

const AUDIO_MIME_TYPE_BY_EXTENSION: &[(&str, &str)] = &[
    (".mp3", "audio/mpeg"),
    (".wav", "audio/wav"),
    (".ogg", "audio/ogg"),
    (".oga", "audio/ogg"),
    (".flac", "audio/flac"),
    (".aac", "audio/aac"),
    (".m4a", "audio/mp4"),
    (".opus", "audio/ogg"),
    (".aiff", "audio/aiff"),
];

const VIDEO_MIME_TYPE_BY_EXTENSION: &[(&str, &str)] = &[
    ("avi", "video/x-msvideo"),
    ("m4v", "video/mp4"),
    ("mkv", "video/x-matroska"),
    ("mov", "video/quicktime"),
    ("mp4", "video/mp4"),
    ("ogv", "video/ogg"),
    ("webm", "video/webm"),
];

/// `GENERIC_MIME_TYPES` (`@t3tools/shared/image`).
const GENERIC_MIME_TYPES: &[&str] = &["application/octet-stream", "binary/octet-stream", "application/unknown"];

fn lookup(table: &[(&str, &'static str)], key: &str) -> Option<&'static str> {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// `/^\.[a-z0-9]+$/i`.
fn is_literal_extension(extension: &str) -> bool {
    extension.len() > 1 && extension.starts_with('.') && extension[1..].bytes().all(|b| b.is_ascii_alphanumeric())
}

/// `videoMimeType({name, mimeType})`.
pub fn video_mime_type(name: &str, mime_type: &str) -> Option<String> {
    let mime = mime_type.split(';').next().unwrap_or("").trim().to_lowercase();
    if mime.starts_with("video/") {
        return Some(mime);
    }
    if !mime.is_empty() && !GENERIC_MIME_TYPES.contains(&mime.as_str()) {
        return None;
    }
    let dot = name.rfind('.')?;
    lookup(VIDEO_MIME_TYPE_BY_EXTENSION, &name[dot + 1..].to_lowercase()).map(str::to_owned)
}

/// `audioMimeTypeFromExtension`.
pub fn audio_mime_type_from_extension(extension: &str) -> Option<&'static str> {
    if !is_literal_extension(extension) {
        return None;
    }
    lookup(AUDIO_MIME_TYPE_BY_EXTENSION, &extension.to_lowercase())
}

/// `mediaMimeTypeFromExtension`: images, then videos.
pub fn media_mime_type_from_extension(extension: &str) -> Option<String> {
    if !is_literal_extension(extension) {
        return None;
    }
    lookup(IMAGE_MIME_TYPE_BY_EXTENSION, &extension.to_lowercase())
        .map(str::to_owned)
        .or_else(|| video_mime_type(&format!("media{extension}"), ""))
}

/// `hostPreviewMimeTypeFromExtension`: what is served in place from anywhere on the host
/// (media, audio, browser documents).
pub fn host_preview_mime_type_from_extension(extension: &str) -> Option<String> {
    if !is_literal_extension(extension) {
        return None;
    }
    media_mime_type_from_extension(extension)
        .or_else(|| audio_mime_type_from_extension(extension).map(str::to_owned))
        .or_else(|| lookup(BROWSER_MIME_TYPE_BY_EXTENSION, &extension.to_lowercase()).map(str::to_owned))
}

fn has_preview_extension(path: &str, extensions: &[&str]) -> bool {
    let without_query = path.split(['?', '#']).next().unwrap_or("").to_lowercase();
    extensions.iter().any(|extension| without_query.ends_with(extension))
}

/// `isWorkspaceBrowserPreviewPath`.
pub fn is_workspace_browser_preview_path(path: &str) -> bool {
    has_preview_extension(path, WORKSPACE_BROWSER_PREVIEW_EXTENSIONS)
}

/// `isWorkspaceImagePreviewPath`.
pub fn is_workspace_image_preview_path(path: &str) -> bool {
    has_preview_extension(path, WORKSPACE_IMAGE_PREVIEW_EXTENSIONS)
}

/// `isWorkspacePreviewEntryPath`.
pub fn is_workspace_preview_entry_path(path: &str) -> bool {
    is_workspace_browser_preview_path(path) || is_workspace_image_preview_path(path)
}

/// `PREVIEW_ASSET_EXTENSIONS` (`AssetAccess.ts`): what a sibling request of a `workspace-file`
/// URL (an HTML report's stylesheet, script, font or picture) may name.
pub fn is_preview_asset_extension(extension: &str) -> bool {
    let extension = extension.to_lowercase();
    WORKSPACE_BROWSER_PREVIEW_EXTENSIONS.contains(&extension.as_str())
        || WORKSPACE_IMAGE_PREVIEW_EXTENSIONS.contains(&extension.as_str())
        || [".css", ".js", ".mjs", ".otf", ".ttf", ".woff", ".woff2"].contains(&extension.as_str())
}

/// The extensions of the Node helpers below are POSIX.
pub use zc_orchestration::attachments::extname;
pub use zc_workspace::paths::{basename, dirname, is_absolute, join, relative, resolve};

/// `path.normalize` (POSIX).
pub fn normalize(path: &str) -> String {
    zc_orchestration::attachments::posix_normalize(path)
}

/// `encodeURIComponent`.
pub fn encode_uri_component(value: &str) -> String {
    zc_auth::cookies::encode_uri_component(value)
}

/// `decodeURIComponent`, or `None` where it throws (a malformed escape, invalid UTF-8).
pub fn decode_uri_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes.get(index + 1..index + 3)?;
            if !hex.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_host_preview_types() {
        assert_eq!(host_preview_mime_type_from_extension(".png").as_deref(), Some("image/png"));
        assert_eq!(host_preview_mime_type_from_extension(".MP4").as_deref(), Some("video/mp4"));
        assert_eq!(host_preview_mime_type_from_extension(".webm").as_deref(), Some("video/webm"));
        assert_eq!(host_preview_mime_type_from_extension(".wav").as_deref(), Some("audio/wav"));
        assert_eq!(host_preview_mime_type_from_extension(".html").as_deref(), Some("text/html"));
        assert_eq!(host_preview_mime_type_from_extension(".pdf").as_deref(), Some("application/pdf"));
        assert_eq!(host_preview_mime_type_from_extension(".md"), None);
        assert_eq!(host_preview_mime_type_from_extension(".%70ng"), None);
        assert_eq!(host_preview_mime_type_from_extension(""), None);
    }

    #[test]
    fn classifies_workspace_preview_paths() {
        assert!(is_workspace_image_preview_path("assets/icon.PNG"));
        assert!(is_workspace_image_preview_path("icon.svg?v=2"));
        // The query and fragment are not part of a workspace path's extension.
        assert!(is_workspace_image_preview_path("secret.png#private.txt"));
        assert!(is_workspace_preview_entry_path("report.html"));
        assert!(!is_workspace_preview_entry_path("report.md"));
        assert!(is_preview_asset_extension(".WOFF2"));
        assert!(!is_preview_asset_extension(".txt"));
    }

    #[test]
    fn video_types_need_a_generic_mime() {
        assert_eq!(video_mime_type("clip.mov", "").as_deref(), Some("video/quicktime"));
        assert_eq!(video_mime_type("clip.mp4", "application/pdf"), None);
        assert_eq!(video_mime_type("x", "video/webm; codecs=vp9").as_deref(), Some("video/webm"));
    }

    #[test]
    fn decodes_uri_components_like_js() {
        assert_eq!(decode_uri_component("100%25.png").as_deref(), Some("100%.png"));
        assert_eq!(decode_uri_component("%E2%82%AC").as_deref(), Some("€"));
        assert_eq!(decode_uri_component("%zz"), None);
        assert_eq!(decode_uri_component("%C3"), None);
        assert_eq!(encode_uri_component("a b/é"), "a%20b%2F%C3%A9");
    }
}
