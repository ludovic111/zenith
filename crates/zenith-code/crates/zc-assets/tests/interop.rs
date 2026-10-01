//! Interop with the TypeScript server: the real `AssetAccess` and `AttachmentUpload` of
//! `code/apps/server` (`ts/asset_oracle.mjs`, through node and the real effect/contracts
//! packages) run over the same base directory, so they share the signing key file and the
//! attachments directory. Every URL the TS side mints must resolve in Rust to what TS resolves
//! it to, and the other way round; upload tokens must validate both ways; and the claims must
//! encode byte for byte alike.
//!
//! Needs `node` and `code/apps/server/node_modules` (symlinked from a checkout that has them);
//! without them the test prints why and passes vacuously.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use common::*;
use serde_json::{json, Map, Value};
use zc_assets::access::{verified_payload, AssetClaims, ResolvedAsset, SIGNING_SECRET_NAME};
use zc_assets::upload::AttachmentUploadClaims;
use zc_assets::IssueAssetUrlInput;
use zc_contracts::{AssetResource, AttachmentCreateUploadUrlInput};

fn server_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../code/apps/server")
}

fn oracle_available() -> Result<(), String> {
    if !server_dir().join("node_modules/effect").exists() {
        return Err(format!("{} has no node_modules", server_dir().display()));
    }
    match Command::new("node").arg("--version").output() {
        Ok(output) if output.status.success() => Ok(()),
        _ => Err("node is not installed".into()),
    }
}

fn run_oracle(base_dir: &Path, cases: &[Value]) -> Map<String, Value> {
    let script = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ts/asset_oracle.mjs")).unwrap();
    let server = std::fs::canonicalize(server_dir()).unwrap();
    let mut child = Command::new("node")
        .args(["--no-warnings", "--input-type=module", "-e", &script])
        .current_dir(&server)
        .env("ZC_SERVER_SRC", server.join("src"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(json!({"baseDir": base_dir, "cases": cases}).to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "oracle failed: {}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| panic!("oracle output: {error}: {}", String::from_utf8_lossy(&output.stdout)))
}

/// The Rust resolution in the oracle's shape.
fn shape(asset: Option<ResolvedAsset>) -> Value {
    match asset {
        None => Value::Null,
        Some(ResolvedAsset::GithubMedia { url, cwd, expires_at }) => {
            json!({"kind": "github-media", "url": url, "cwd": cwd, "expiresAt": zc_contracts::JsNumber(expires_at)})
        }
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
            if file.file.is_some() {
                value["opened"] = json!(true);
            }
            value
        }
    }
}

fn ok(results: &Map<String, Value>, id: &str) -> Value {
    let result = &results[id];
    assert!(result.get("error").is_none(), "{id}: {result}");
    result["ok"].clone()
}

#[tokio::test]
async fn urls_and_upload_tokens_cross_between_the_ts_and_rust_servers() {
    if let Err(reason) = oracle_available() {
        eprintln!("skipped: {reason}");
        return;
    }
    let f = Fixture::new().await;
    // The Rust side creates the key; the TS side reads the same file.
    f.secrets.get_or_create_random(SIGNING_SECRET_NAME, 32).await.unwrap();

    let project = f.mkdir("project");
    write(format!("{project}/public/favicon.svg"), "<svg>icon</svg>");
    write(format!("{project}/report.html"), "<p>report</p>");
    write(format!("{project}/report.css"), "p { color: red; }");
    let media = f.mkdir("media");
    write(format!("{media}/clip.mp4"), "video bytes");
    let png = [
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, 0x49, 0x48, 0x44, 0x52, 0, 0, 0x06, 0x40, 0, 0, 0x03, 0x84,
    ];
    write(format!("{media}/shot.png"), png);
    let attachment_id = "thread-1-00000000-0000-4000-8000-000000000007-pdf";
    write(f.attachments_dir.join(format!("{attachment_id}.pdf")), "%PDF");

    let resources: Vec<(&str, Value, Option<String>, Option<String>)> = vec![
        ("favicon", json!({"_tag": "project-favicon", "cwd": project}), None, None),
        (
            "favicon-saved",
            json!({"_tag": "project-favicon", "cwd": project}),
            None,
            Some(format!("{media}/shot.png")),
        ),
        (
            "media",
            json!({"_tag": "media-file", "threadId": "thread-1", "path": format!("{media}/clip.mp4")}),
            None,
            None,
        ),
        (
            "image",
            json!({"_tag": "media-file", "threadId": "thread-1", "path": "shot.png"}),
            Some(media.clone()),
            None,
        ),
        (
            "workspace",
            json!({"_tag": "workspace-file", "threadId": "thread-1", "path": "report.html"}),
            Some(project.clone()),
            None,
        ),
        (
            "attachment",
            json!({"_tag": "attachment", "attachmentId": attachment_id, "fileName": "report.pdf", "mimeType": "application/pdf"}),
            None,
            None,
        ),
        (
            "github",
            json!({"_tag": "github-media", "cwd": project, "url": "https://github.com/owner/repo/blob/main/docs/shot.png"}),
            None,
            None,
        ),
    ];

    // 1. Both sides mint every resource.
    let mut cases = Vec::new();
    for (id, resource, workspace_root, saved) in &resources {
        let mut case = json!({"id": format!("issue:{id}"), "op": "issue", "resource": resource});
        if let Some(root) = workspace_root {
            case["workspaceRoot"] = json!(root);
        }
        if let Some(saved) = saved {
            case["projectFaviconPath"] = json!(saved);
        }
        cases.push(case);
    }
    cases.push(json!({"id": "upload:image", "op": "issueUpload", "input": {"name": "paste.png", "mimeType": "image/png", "sizeBytes": 6}}));
    cases.push(json!({"id": "upload:file", "op": "issueUpload", "input": {"type": "file", "name": "notes.md", "mimeType": "text/markdown", "sizeBytes": 9}}));
    let ts_issued = run_oracle(&f.base_dir, &cases);

    let mut rust_urls = Vec::new();
    for (id, resource, workspace_root, saved) in &resources {
        let resource: AssetResource = serde_json::from_value(resource.clone()).unwrap();
        let issued = f
            .assets
            .access
            .issue_asset_url(IssueAssetUrlInput {
                resource,
                workspace_root: workspace_root.clone(),
                project_favicon_path: saved.clone(),
            })
            .await
            .unwrap_or_else(|error| panic!("{id}: {error:?}"));
        let ts = ok(&ts_issued, &format!("issue:{id}"));
        // Same file name, same source path and image size.
        let name = |url: &str| url.rsplit('/').next().unwrap().to_owned();
        assert_eq!(name(&issued.relative_url), name(ts["relativeUrl"].as_str().unwrap()), "{id}");
        assert_eq!(issued.source_path.as_deref(), ts.get("sourcePath").and_then(Value::as_str), "{id}");
        assert_eq!(
            serde_json::to_value(&issued.image_dimensions).unwrap(),
            ts.get("imageDimensions").cloned().unwrap_or(Value::Null),
            "{id}"
        );
        rust_urls.push((id.to_string(), issued.relative_url, ts["relativeUrl"].as_str().unwrap().to_owned()));
    }

    // 2. The claims: TS's decode in Rust and re-encode byte for byte; both sides agree on them.
    let secret = f.secrets.get_or_create_random(SIGNING_SECRET_NAME, 32).await.unwrap();
    for (id, rust_url, ts_url) in &rust_urls {
        let (ts_token, _) = split_url(ts_url);
        let (rust_token, _) = split_url(rust_url);
        let ts_json = verified_payload(&ts_token, &secret).unwrap_or_else(|| panic!("{id}: TS token does not verify in Rust"));
        let ts_claims = AssetClaims::from_json(&ts_json).unwrap_or_else(|| panic!("{id}: {ts_json}"));
        assert_eq!(ts_claims.to_json(), ts_json, "{id}");
        let rust_claims = AssetClaims::from_json(&verified_payload(&rust_token, &secret).unwrap()).unwrap();
        let without_expiry = |claims: &AssetClaims| {
            let mut value: Value = serde_json::from_str(&claims.to_json()).unwrap();
            value.as_object_mut().unwrap().remove("expiresAt");
            value
        };
        assert_eq!(without_expiry(&rust_claims), without_expiry(&ts_claims), "{id}");
    }

    // 3. Each side resolves the other's URLs exactly like its own.
    let mut cases = Vec::new();
    for (id, rust_url, ts_url) in &rust_urls {
        for (side, url) in [("rust", rust_url), ("ts", ts_url)] {
            let (token, name) = split_url(url);
            cases.push(json!({"id": format!("resolve:{side}:{id}"), "op": "resolve", "token": token, "name": name}));
        }
    }
    for side in ["image", "file"] {
        let issued = ok(&ts_issued, &format!("upload:{side}"));
        let token = issued["relativeUrl"].as_str().unwrap().rsplit('/').next().unwrap().to_owned();
        let claims = f
            .assets
            .uploads
            .validate_upload_token(&token)
            .await
            .unwrap_or_else(|| panic!("TS {side} upload token does not validate in Rust"));
        assert_eq!(claims.attachment_id, issued["attachmentId"].as_str().unwrap());
        let input: AttachmentCreateUploadUrlInput = serde_json::from_value(if side == "image" {
            json!({"name": "paste.png", "mimeType": "image/png", "sizeBytes": 6})
        } else {
            json!({"type": "file", "name": "notes.md", "mimeType": "text/markdown", "sizeBytes": 9})
        })
        .unwrap();
        let rust_issued = f.assets.uploads.issue_upload_url(&input).await.unwrap();
        let rust_token = rust_issued.relative_url.rsplit('/').next().unwrap().to_owned();
        let ts_payload = verified_payload(&token, &secret).unwrap();
        assert_eq!(AttachmentUploadClaims::from_json(&ts_payload).unwrap().to_json(), ts_payload, "{side}");
        cases.push(json!({"id": format!("validate:{side}"), "op": "validateUpload", "token": rust_token, "attachmentId": rust_issued.attachment_id}));
    }
    let ts_resolved = run_oracle(&f.base_dir, &cases);
    for (id, rust_url, ts_url) in &rust_urls {
        for (side, url) in [("rust", rust_url), ("ts", ts_url)] {
            let (token, name) = split_url(url);
            let rust = shape(f.assets.access.resolve_asset(&token, &name).await);
            let ts = ok(&ts_resolved, &format!("resolve:{side}:{id}"));
            assert!(!rust.is_null(), "{side} URL of {id} does not resolve in Rust");
            assert_eq!(rust, ts, "{side} URL of {id}");
        }
    }
    for side in ["image", "file"] {
        let ts = ok(&ts_resolved, &format!("validate:{side}"));
        assert_eq!(ts["kind"], "attachment-upload", "Rust {side} upload token does not validate in TS: {ts}");
        assert_eq!(ts["type"], side);
    }
}
