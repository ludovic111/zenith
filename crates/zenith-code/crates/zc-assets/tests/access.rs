//! `AssetAccess.test.ts`, case for case where the Rust design allows it. The cases that inject
//! a swapped file system or patch `node:fs/promises` (a symlink swapped between the canonical
//! check and the open, an ancestor directory race, a rejected descriptor being closed) are
//! covered by the `media_file` unit tests and by Rust's ownership of the descriptor.

#![allow(clippy::result_large_err)]

mod common;

use std::sync::Arc;

use common::*;
use serde_json::{json, Value};
use zc_assets::access::ResolvedAsset;
use zc_assets::favicon::FaviconFs;
use zc_assets::http::asset_file_response;
use zc_assets::preview::PROJECT_FAVICON_FALLBACK_MARKER;
use zc_assets::{IssueAssetUrlInput, ProjectFaviconResolver, ResolvedFile};
use zc_contracts::{AssetAccessError, AssetCreateUrlResult, AssetResource};

fn resource(value: Value) -> AssetResource {
    serde_json::from_value(value).unwrap()
}

async fn issue(f: &Fixture, resource_json: Value, workspace_root: Option<&str>) -> Result<AssetCreateUrlResult, AssetAccessError> {
    f.assets
        .access
        .issue_asset_url(IssueAssetUrlInput {
            resource: resource(resource_json),
            workspace_root: workspace_root.map(str::to_owned),
            project_favicon_path: None,
        })
        .await
}

async fn issue_favicon(f: &Fixture, cwd: &str, path_hint: Option<&str>, saved: Option<&str>) -> Result<AssetCreateUrlResult, AssetAccessError> {
    let mut resource_json = json!({"_tag": "project-favicon", "cwd": cwd});
    if let Some(hint) = path_hint {
        resource_json["path"] = json!(hint);
    }
    f.assets
        .access
        .issue_asset_url(IssueAssetUrlInput {
            resource: resource(resource_json),
            workspace_root: None,
            project_favicon_path: saved.map(str::to_owned),
        })
        .await
}

async fn resolve(f: &Fixture, token: &str, name: &str) -> Option<ResolvedAsset> {
    f.assets.access.resolve_asset(token, name).await
}

async fn resolve_url(f: &Fixture, relative_url: &str) -> Option<ResolvedAsset> {
    let (token, name) = split_url(relative_url);
    resolve(f, &token, &name).await
}

fn file(asset: Option<ResolvedAsset>) -> Option<ResolvedFile> {
    match asset? {
        ResolvedAsset::File(file) => Some(file),
        ResolvedAsset::GithubMedia { .. } => None,
    }
}

/// `{kind, path, …}` like the TS `toEqual` shapes (the descriptor left out).
fn shape(asset: Option<ResolvedAsset>) -> Value {
    match asset {
        None => Value::Null,
        Some(ResolvedAsset::GithubMedia { url, cwd, expires_at }) => json!({"kind": "github-media", "url": url, "cwd": cwd, "expiresAt": expires_at}),
        Some(ResolvedAsset::File(file)) => {
            let mut value = json!({"kind": "file", "path": file.path});
            if file.download {
                value["download"] = json!(true);
            }
            if let Some(name) = file.file_name {
                value["fileName"] = json!(name);
            }
            if let Some(mime) = file.mime_type {
                value["mimeType"] = json!(mime);
            }
            value
        }
    }
}

fn tag(error: &AssetAccessError) -> String {
    serde_json::to_value(error).unwrap()["_tag"].as_str().unwrap().to_owned()
}

async fn body_text(response: axum::response::Response) -> String {
    use http_body_util::BodyExt as _;
    String::from_utf8(response.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
}

#[tokio::test]
async fn issues_exact_urls_for_media_and_documents_outside_the_workspace() {
    let f = Fixture::new().await;
    let root = f.mkdir("media-root");
    let outside = f.mkdir("media-outside");
    for (name, mime_type) in [
        ("screenshot.png", "image/png"),
        ("recording.mp4", "video/mp4"),
        ("recording.webm", "video/webm"),
        ("report.html", "text/html"),
        ("report.pdf", "application/pdf"),
    ] {
        let file_path = format!("{outside}/{name}");
        write(&file_path, "media");
        let result = issue(&f, json!({"_tag": "media-file", "threadId": "thread-1", "path": file_path}), Some(&root))
            .await
            .unwrap();
        let (token, url_name) = split_url(&result.relative_url);
        assert_eq!(
            shape(resolve(&f, &token, &url_name).await),
            json!({"kind": "file", "path": real(&file_path), "mimeType": mime_type})
        );
        write(format!("{outside}/sibling.png"), "private sibling");
        assert!(resolve(&f, &token, "sibling.png").await.is_none());
        assert!(resolve(&f, &token, &format!("../{name}")).await.is_none());
        assert!(resolve(&f, &format!("{token}tampered"), name).await.is_none());
    }
}

#[tokio::test]
async fn reports_pixel_dimensions_from_an_image_header_only() {
    let f = Fixture::new().await;
    let root = f.mkdir("media-dimensions");
    let png = [
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, 0x49, 0x48, 0x44, 0x52, 0, 0, 0x06, 0x40, 0, 0, 0x03, 0x84,
    ];
    write(format!("{root}/shot.png"), png);
    write(format!("{root}/clip.mp4"), "video");
    write(format!("{root}/broken.png"), "not a png");
    let dims = |result: AssetCreateUrlResult| result.image_dimensions.map(|d| (d.width, d.height));
    let issue_name = |name: &str| issue(&f, json!({"_tag": "media-file", "threadId": "thread-1", "path": name}), Some(&root));
    assert_eq!(dims(issue_name("shot.png").await.unwrap()), Some((1600, 900)));
    assert_eq!(dims(issue_name("clip.mp4").await.unwrap()), None);
    assert_eq!(dims(issue_name("broken.png").await.unwrap()), None);
}

#[tokio::test]
async fn resolves_relative_media_paths_from_the_thread_workspace_including_outside_it() {
    let f = Fixture::new().await;
    let directory = f.mkdir("media-relative");
    let root = f.mkdir("media-relative/workspace");
    for relative_path in ["screenshot.png", "../recording.mp4"] {
        let file_path = zc_assets::preview::resolve(&root, relative_path);
        write(&file_path, "media");
        let result = issue(&f, json!({"_tag": "media-file", "threadId": "thread-1", "path": relative_path}), Some(&root))
            .await
            .unwrap();
        assert_eq!(file(resolve_url(&f, &result.relative_url).await).unwrap().path, real(&file_path));
    }
    let _ = directory;
    // Without a workspace, a relative media path has no context.
    let error = issue(&f, json!({"_tag": "media-file", "threadId": "thread-1", "path": "screenshot.png"}), None)
        .await
        .unwrap_err();
    assert_eq!(tag(&error), "AssetWorkspaceContextNotFoundError");
}

#[tokio::test]
async fn rejects_non_previewable_files_disguised_targets_and_directories() {
    let f = Fixture::new().await;
    let root = f.mkdir("media-validation");
    for name in ["report.md", "secret.txt", "secret.%70ng", "secret.png#private.txt"] {
        let file_path = format!("{root}/{name}");
        write(&file_path, "not media");
        let error = issue(&f, json!({"_tag": "media-file", "threadId": "thread-1", "path": file_path}), None)
            .await
            .unwrap_err();
        assert_eq!(tag(&error), "AssetPreviewTypeValidationError", "{name}");
    }
    let disguised = format!("{root}/disguised.png");
    std::os::unix::fs::symlink(format!("{root}/secret.txt"), &disguised).unwrap();
    let error = issue(&f, json!({"_tag": "media-file", "threadId": "thread-1", "path": disguised}), None)
        .await
        .unwrap_err();
    assert_eq!(tag(&error), "AssetPreviewTypeValidationError");
    let directory = format!("{root}/directory.png");
    std::fs::create_dir(&directory).unwrap();
    let error = issue(&f, json!({"_tag": "media-file", "threadId": "thread-1", "path": directory}), None)
        .await
        .unwrap_err();
    assert_eq!(tag(&error), "AssetWorkspaceAssetNotFoundError");
}

#[tokio::test]
async fn binds_media_urls_to_the_canonical_target() {
    let f = Fixture::new().await;
    let root = f.mkdir("media-symlink");
    let file_path = format!("{root}/actual.svg");
    let alias = format!("{root}/alias.png");
    let replacement = format!("{root}/other.svg");
    write(&file_path, "<svg/>");
    write(&replacement, "<svg>private</svg>");
    std::os::unix::fs::symlink(&file_path, &alias).unwrap();
    let result = issue(&f, json!({"_tag": "media-file", "threadId": "thread-1", "path": alias}), None)
        .await
        .unwrap();
    let (token, name) = split_url(&result.relative_url);
    let expected = json!({"kind": "file", "path": real(&file_path), "mimeType": "image/svg+xml"});
    assert_eq!(shape(resolve(&f, &token, &name).await), expected);
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&replacement, &alias).unwrap();
    assert_eq!(shape(resolve(&f, &token, &name).await), expected);
    std::fs::remove_file(&file_path).unwrap();
    std::os::unix::fs::symlink(&replacement, &file_path).unwrap();
    assert!(resolve(&f, &token, &name).await.is_none());
}

#[tokio::test]
async fn keeps_full_and_partial_responses_bound_to_the_file_opened_during_resolution() {
    let f = Fixture::new().await;
    let root = f.mkdir("media-open-file");
    let file_path = format!("{root}/recording.mp4");
    let saved = format!("{root}/saved.mp4");
    let secret = format!("{root}/secret.txt");
    write(&file_path, "0123456789");
    write(&secret, "private information");
    let result = issue(&f, json!({"_tag": "media-file", "threadId": "thread-1", "path": file_path}), None)
        .await
        .unwrap();
    for (range, expected, status) in [(None, "0123456789", 200), (Some("bytes=2-5"), "2345", 206)] {
        let asset = file(resolve_url(&f, &result.relative_url).await).expect("a resolved media file");
        std::fs::rename(&file_path, &saved).unwrap();
        std::os::unix::fs::symlink(&secret, &file_path).unwrap();
        let response = asset_file_response(asset, range, None, &http::Method::GET).await.unwrap();
        assert_eq!(response.status().as_u16(), status);
        assert_eq!(response.headers()["content-length"], expected.len().to_string());
        assert_eq!(body_text(response).await, expected);
        std::fs::remove_file(&file_path).unwrap();
        std::fs::rename(&saved, &file_path).unwrap();
    }
}

#[tokio::test]
async fn keeps_in_place_edits_readable_but_requires_a_new_url_after_replacement() {
    let f = Fixture::new().await;
    let root = f.mkdir("media-replacement");
    let file_path = format!("{root}/recording.mp4");
    write(&file_path, "original");
    let input = json!({"_tag": "media-file", "threadId": "thread-1", "path": file_path});
    let original = issue(&f, input.clone(), None).await.unwrap();
    std::fs::OpenOptions::new().write(true).truncate(true).open(&file_path).unwrap();
    std::fs::write(&file_path, "in-place edit").unwrap();
    let edited = file(resolve_url(&f, &original.relative_url).await).expect("the edited media file");
    assert_eq!(
        body_text(asset_file_response(edited, None, None, &http::Method::GET).await.unwrap()).await,
        "in-place edit"
    );

    let replacement = format!("{root}/replacement.mp4");
    write(&replacement, "replacement");
    std::fs::rename(&replacement, &file_path).unwrap();
    assert!(resolve_url(&f, &original.relative_url).await.is_none());

    let renewed = issue(&f, input, None).await.unwrap();
    let renewed_asset = file(resolve_url(&f, &renewed.relative_url).await).expect("the replacement media file");
    assert_eq!(
        body_text(asset_file_response(renewed_asset, None, None, &http::Method::GET).await.unwrap()).await,
        "replacement"
    );
    std::fs::remove_file(&file_path).unwrap();
    assert!(resolve_url(&f, &renewed.relative_url).await.is_none());
}

#[tokio::test]
async fn issues_workspace_urls_for_the_entry_and_its_sibling_assets() {
    let f = Fixture::new().await;
    let root = f.mkdir("asset-workspace");
    write(format!("{root}/report.html"), "<link rel=\"stylesheet\" href=\"report.css\">");
    write(format!("{root}/report.css"), "body { color: red; }");
    write(format!("{root}/.env"), "SECRET=value");
    let result = issue(
        &f,
        json!({"_tag": "workspace-file", "threadId": "thread-1", "path": format!("{root}/report.html")}),
        Some(&root),
    )
    .await
    .unwrap();
    let (token, _) = split_url(&result.relative_url);
    assert_eq!(
        shape(resolve(&f, &token, "report.html").await),
        json!({"kind": "file", "path": real(format!("{root}/report.html"))})
    );
    assert_eq!(
        shape(resolve(&f, &token, "report.css").await),
        json!({"kind": "file", "path": real(format!("{root}/report.css"))})
    );
    assert!(resolve(&f, &token, "../secret.txt").await.is_none());
    assert!(resolve(&f, &token, ".env").await.is_none());
    assert!(resolve(&f, &format!("{token}tampered"), "report.html").await.is_none());
}

#[tokio::test]
async fn rejects_workspace_files_outside_the_authorized_root() {
    let f = Fixture::new().await;
    let root = f.mkdir("asset-root");
    let outside = f.mkdir("asset-outside");
    let html = format!("{outside}/report.html");
    write(&html, "<p>outside</p>");
    let error = issue(&f, json!({"_tag": "workspace-file", "threadId": "thread-1", "path": html}), Some(&root))
        .await
        .unwrap_err();
    let encoded = serde_json::to_value(&error).unwrap();
    assert_eq!(encoded["_tag"], "AssetWorkspacePathValidationError");
    assert_eq!(encoded["resource"], json!({"_tag": "workspace-file", "threadId": "thread-1", "path": html}));
    assert_eq!(encoded["cause"]["name"], "WorkspacePathOutsideRootError");
}

#[tokio::test]
async fn issues_draft_workspace_urls_without_a_thread() {
    let f = Fixture::new().await;
    let root = f.mkdir("asset-draft");
    write(format!("{root}/report.html"), "<link rel=\"stylesheet\" href=\"report.css\">");
    write(format!("{root}/report.css"), "body { color: red; }");
    for workspace_root in [Some(root.as_str()), None] {
        let result = issue(&f, json!({"_tag": "draft-workspace-file", "cwd": root, "path": "report.html"}), workspace_root)
            .await
            .unwrap();
        let (token, _) = split_url(&result.relative_url);
        assert_eq!(
            shape(resolve(&f, &token, "report.html").await),
            json!({"kind": "file", "path": real(format!("{root}/report.html"))})
        );
        assert_eq!(
            shape(resolve(&f, &token, "report.css").await),
            json!({"kind": "file", "path": real(format!("{root}/report.css"))})
        );
        assert!(resolve(&f, &token, "../secret.txt").await.is_none());
    }
}

#[tokio::test]
async fn serves_absolute_draft_media_exactly() {
    let f = Fixture::new().await;
    let root = f.mkdir("asset-draft-root");
    let outside = f.mkdir("asset-draft-outside");
    let clip = format!("{outside}/clip.mp4");
    write(&clip, "video");
    let result = issue(&f, json!({"_tag": "draft-workspace-file", "cwd": root, "path": clip}), Some(&root))
        .await
        .unwrap();
    let (token, _) = split_url(&result.relative_url);
    assert_eq!(
        shape(resolve(&f, &token, "clip.mp4").await),
        json!({"kind": "file", "path": real(&clip), "mimeType": "video/mp4"})
    );
    assert!(resolve(&f, &token, "other.mp4").await.is_none());
}

#[tokio::test]
async fn preserves_non_missing_canonical_path_failures() {
    use std::os::unix::fs::PermissionsExt as _;
    let f = Fixture::new().await;
    let root = f.mkdir("asset-permission-root");
    write(format!("{root}/locked/report.html"), "<p>report</p>");
    std::fs::set_permissions(format!("{root}/locked"), std::fs::Permissions::from_mode(0o000)).unwrap();
    let result = issue(
        &f,
        json!({"_tag": "workspace-file", "threadId": "thread-1", "path": "locked/report.html"}),
        Some(&root),
    )
    .await;
    std::fs::set_permissions(format!("{root}/locked"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let encoded = serde_json::to_value(result.unwrap_err()).unwrap();
    assert_eq!(encoded["_tag"], "AssetWorkspaceAssetInspectionError");
    assert_eq!(encoded["cause"]["name"], "PlatformError");
    assert!(encoded["cause"]["message"]
        .as_str()
        .unwrap()
        .starts_with("PermissionDenied: FileSystem.realPath"));
}

#[tokio::test]
async fn issues_exact_workspace_urls_for_image_previews() {
    let f = Fixture::new().await;
    let root = f.mkdir("asset-image-workspace");
    write(format!("{root}/assets/icon.png"), [137, 80, 78, 71]);
    write(format!("{root}/assets/other.png"), [137, 80, 78, 71]);
    let result = issue(
        &f,
        json!({"_tag": "workspace-file", "threadId": "thread-1", "path": format!("{root}/assets/icon.png")}),
        Some(&root),
    )
    .await
    .unwrap();
    let (token, _) = split_url(&result.relative_url);
    assert_eq!(
        shape(resolve(&f, &token, "icon.png").await),
        json!({"kind": "file", "path": real(format!("{root}/assets/icon.png"))})
    );
    assert!(resolve(&f, &token, "other.png").await.is_none());
    assert!(resolve(&f, &token, "../icon.png").await.is_none());
}

#[tokio::test]
async fn issues_attachment_capabilities_by_id() {
    let f = Fixture::new().await;
    let image_id = "thread-1-00000000-0000-4000-8000-000000000001";
    let image_path = f.attachments_dir.join(format!("{image_id}.png"));
    write(&image_path, [1, 2, 3]);
    let result = issue(&f, json!({"_tag": "attachment", "attachmentId": image_id}), None).await.unwrap();
    let (token, _) = split_url(&result.relative_url);
    assert_eq!(
        shape(resolve(&f, &token, "ignored.png").await),
        json!({"kind": "file", "path": image_path.to_string_lossy()})
    );

    // Videos render inline, with the essence of their type.
    let video_id = "thread-1-00000000-0000-4000-8000-000000000001-mp4";
    let video_path = f.attachments_dir.join(format!("{video_id}.mp4"));
    write(&video_path, [1, 2, 3]);
    let result = issue(
        &f,
        json!({"_tag": "attachment", "attachmentId": video_id, "fileName": "demo.mp4", "mimeType": "video/mp4; codecs=\"avc1.42E01E\""}),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        shape(resolve_url(&f, &result.relative_url).await),
        json!({"kind": "file", "path": video_path.to_string_lossy(), "fileName": "demo.mp4", "mimeType": "video/mp4"})
    );

    // Documents inline when a viewer asks.
    let pdf_id = "thread-1-00000000-0000-4000-8000-000000000001-pdf";
    let pdf_path = f.attachments_dir.join(format!("{pdf_id}.pdf"));
    write(&pdf_path, [1, 2, 3]);
    let result = issue(
        &f,
        json!({"_tag": "attachment", "attachmentId": pdf_id, "fileName": "report.pdf", "mimeType": "application/pdf", "disposition": "inline"}),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        shape(resolve_url(&f, &result.relative_url).await),
        json!({"kind": "file", "path": pdf_path.to_string_lossy(), "fileName": "report.pdf", "mimeType": "application/pdf"})
    );

    // Audio previews use the stored format; saving stays explicit.
    let wav_id = "thread-1-00000000-0000-4000-8000-000000000003-wav";
    let wav_path = f.attachments_dir.join(format!("{wav_id}.wav"));
    write(&wav_path, [1, 2, 3]);
    for disposition in ["inline", "attachment"] {
        let result = issue(
            &f,
            json!({"_tag": "attachment", "attachmentId": wav_id, "fileName": "recording.wav", "mimeType": "application/octet-stream", "disposition": disposition}),
            None,
        )
        .await
        .unwrap();
        let mut expected = json!({"kind": "file", "path": wav_path.to_string_lossy(), "fileName": "recording.wav",
            "mimeType": if disposition == "inline" { "audio/wav" } else { "application/octet-stream" }});
        if disposition == "attachment" {
            expected["download"] = json!(true);
        }
        assert_eq!(shape(resolve_url(&f, &result.relative_url).await), expected);
    }

    // Other types stay downloads even when asked inline.
    let zip_id = "thread-1-00000000-0000-4000-8000-000000000002-zip";
    write(f.attachments_dir.join(format!("{zip_id}.zip")), [1, 2, 3]);
    let result = issue(
        &f,
        json!({"_tag": "attachment", "attachmentId": zip_id, "fileName": "archive.zip", "mimeType": "text/html", "disposition": "inline"}),
        None,
    )
    .await
    .unwrap();
    assert!(file(resolve_url(&f, &result.relative_url).await).unwrap().download);

    let error = issue(
        &f,
        json!({"_tag": "attachment", "attachmentId": "thread-1-00000000-0000-4000-8000-0000000000ff"}),
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(tag(&error), "AssetAttachmentNotFoundError");
}

#[tokio::test]
async fn issues_signed_native_application_icon_capabilities() {
    let f = Fixture::new().await;
    let result = issue(
        &f,
        json!({"_tag": "native-app-icon", "app": {"_tag": "app-id", "appId": "com.example.Editor"}}),
        None,
    )
    .await
    .unwrap();
    let pattern = regex::Regex::new(r"^/api/assets/[^/]+/native-app-icon\.png$").unwrap();
    assert!(pattern.is_match(&result.relative_url), "{}", result.relative_url);
    assert!(result.expires_at.get() > 0.0);
}

#[tokio::test]
async fn issues_project_favicon_capabilities_with_a_signed_fallback() {
    let f = Fixture::new().await;
    let root = f.mkdir("asset-favicon");
    let favicon = format!("{root}/favicon.svg");
    write(&favicon, "<svg>a</svg>");
    let first = issue_favicon(&f, &root, None, None).await.unwrap();
    assert_eq!(first.source_path.as_deref(), Some("favicon.svg"));
    assert!(regex::Regex::new(r"/v[0-9a-f]{64}-favicon\.svg$").unwrap().is_match(&first.relative_url));
    assert_eq!(issue_favicon(&f, &root, None, None).await.unwrap(), first);
    assert_eq!(
        shape(resolve_url(&f, &first.relative_url).await),
        json!({"kind": "file", "path": real(&favicon)})
    );

    write(&favicon, "<svg>b</svg>");
    let updated = issue_favicon(&f, &root, None, None).await.unwrap();
    let last_segment = |url: &str| url[url.rfind('/').unwrap()..].to_owned();
    assert_ne!(last_segment(&updated.relative_url), last_segment(&first.relative_url));

    std::fs::remove_file(&favicon).unwrap();
    let fallback = issue_favicon(&f, &root, None, None).await.unwrap();
    assert!(fallback.relative_url.ends_with(&format!("/{PROJECT_FAVICON_FALLBACK_MARKER}")));
    assert_eq!(fallback.source_path, None);
    assert!(resolve_url(&f, &fallback.relative_url).await.is_none());
}

#[tokio::test]
async fn issues_project_favicons_for_saved_overrides() {
    let f = Fixture::new().await;
    let root = f.mkdir("asset-favicon-override");
    write(format!("{root}/brand/custom.svg"), "<svg />");
    write(format!("{root}/favicon.svg"), "<svg>auto</svg>");
    let result = issue_favicon(&f, &root, None, Some("brand/custom.svg")).await.unwrap();
    assert_eq!(result.source_path.as_deref(), Some("brand/custom.svg"));
    assert!(regex::Regex::new(r"/v[0-9a-f]{64}-custom\.svg$").unwrap().is_match(&result.relative_url));

    // The client's path is a cache-key hint only; the saved path decides.
    write(format!("{root}/brand/hint.svg"), "<svg>hint</svg>");
    write(format!("{root}/brand/saved.svg"), "<svg>saved</svg>");
    let result = issue_favicon(&f, &root, Some("brand/hint.svg"), Some("brand/saved.svg")).await.unwrap();
    assert_eq!(result.source_path.as_deref(), Some("brand/saved.svg"));

    // Automatic resolution is cached separately from a saved override.
    let result = issue_favicon(&f, &root, None, None).await.unwrap();
    assert_eq!(result.source_path.as_deref(), Some("favicon.svg"));

    write(format!("{root}/secret.txt"), "not an image");
    let error = issue_favicon(&f, &root, None, Some("secret.txt")).await.unwrap_err();
    assert_eq!(tag(&error), "AssetPreviewTypeValidationError");
}

#[tokio::test]
async fn issues_an_exact_capability_for_a_saved_favicon_outside_the_workspace() {
    let f = Fixture::new().await;
    let root = f.mkdir("asset-favicon-workspace");
    let pictures = f.mkdir("asset-favicon-pictures");
    let external = format!("{pictures}/custom.png");
    write(&external, [1, 2, 3]);
    write(format!("{pictures}/sibling.png"), [4, 5, 6]);
    let result = issue_favicon(&f, &root, None, Some(&external)).await.unwrap();
    let (token, name) = split_url(&result.relative_url);
    assert_eq!(result.source_path.as_deref(), Some(external.as_str()));
    assert!(name.starts_with('v') && name.ends_with("-custom.png"));
    assert_eq!(shape(resolve(&f, &token, &name).await), json!({"kind": "file", "path": real(&external)}));
    assert_eq!(
        shape(resolve(&f, &token, "sibling.png").await),
        json!({"kind": "file", "path": real(&external)})
    );
}

#[tokio::test]
async fn buckets_project_favicon_expiry() {
    let f = Fixture::new().await;
    let root = f.mkdir("asset-favicon-expiry");
    write(format!("{root}/favicon.svg"), "<svg />");
    let bucket = 30 * 60 * 1000;
    f.clock.set(1_000 * bucket - 1);
    assert_eq!(issue_favicon(&f, &root, None, None).await.unwrap().expires_at.get(), (1_001 * bucket) as f64);
    f.clock.set(1_000 * bucket);
    assert_eq!(issue_favicon(&f, &root, None, None).await.unwrap().expires_at.get(), (1_002 * bucket) as f64);
}

#[tokio::test]
async fn preserves_structured_project_favicon_resolution_causes() {
    let failing = ProjectFaviconResolver::new(FaviconFs {
        stat: Arc::new(|_| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))),
        read_to_string: FaviconFs::default().read_to_string,
    });
    let f = Fixture::with_favicons(failing).await;
    let root = f.mkdir("asset-favicon-error");
    let encoded = serde_json::to_value(issue_favicon(&f, &root, None, None).await.unwrap_err()).unwrap();
    assert_eq!(encoded["_tag"], "AssetProjectFaviconResolutionError");
    assert_eq!(encoded["cause"]["name"], "ProjectFaviconResolutionError");
    assert_eq!(
        encoded["cause"]["message"],
        format!("Failed to resolve project favicon during stat-candidate for workspace {root}.")
    );
}

#[tokio::test]
async fn serves_github_hosted_pull_request_media() {
    let f = Fixture::new().await;
    let issue_url = |url: &str| issue(&f, json!({"_tag": "github-media", "cwd": "/repo", "url": url}), None);
    let attachment = issue_url("https://github.com/user-attachments/assets/1a1842fb-6383-492f-873c-57aa0033fa6c")
        .await
        .unwrap();
    assert!(attachment.relative_url.ends_with("/1a1842fb-6383-492f-873c-57aa0033fa6c"));
    assert_eq!(
        shape(resolve_url(&f, &attachment.relative_url).await),
        json!({"kind": "github-media", "url": "https://github.com/user-attachments/assets/1a1842fb-6383-492f-873c-57aa0033fa6c", "cwd": "/repo", "expiresAt": attachment.expires_at.get()})
    );
    let committed = issue_url("https://github.com/owner/repo/blob/main/docs/shot.png").await.unwrap();
    assert_eq!(
        shape(resolve_url(&f, &committed.relative_url).await)["url"],
        "https://raw.githubusercontent.com/owner/repo/main/docs/shot.png"
    );
    let awkward = issue_url("https://raw.githubusercontent.com/o/r/main/100%.png").await.unwrap();
    assert!(awkward.relative_url.ends_with("/100%25.png"));
    for url in [
        "https://example.com/shot.png",
        "https://example.com/shot.png?token=private-media-token",
        "http://github.com/user-attachments/assets/1a1842fb",
        "https://github.com/owner/repo/pull/1",
        "https://github.com/owner/repo/blob/main/",
    ] {
        let error = issue_url(url).await.unwrap_err();
        let encoded = serde_json::to_string(&error).unwrap();
        assert_eq!(encoded, r#"{"_tag":"AssetGitHubMediaUrlValidationError"}"#);
        assert!(!encoded.contains(url));
    }
}

#[tokio::test]
async fn expired_urls_resolve_to_nothing() {
    let f = Fixture::new().await;
    let root = f.mkdir("expiry");
    write(format!("{root}/shot.png"), "png");
    let result = issue(
        &f,
        json!({"_tag": "media-file", "threadId": "thread-1", "path": format!("{root}/shot.png")}),
        None,
    )
    .await
    .unwrap();
    assert!(resolve_url(&f, &result.relative_url).await.is_some());
    f.clock.advance(60 * 60 * 1000);
    assert!(resolve_url(&f, &result.relative_url).await.is_none());
}
