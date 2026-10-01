//! The raw routes, through the axum router (the asset and upload cases of `server.test.ts`,
//! plus the response policy of `http.ts`).

mod common;

use axum::body::Body;
use common::*;
use http::{Method, Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::json;
use tower::ServiceExt as _;
use zc_assets::rpc::{create_upload_url, delete};
use zc_assets::IssueAssetUrlInput;
use zc_contracts::{AssetResource, AttachmentCreateUploadUrlInput, AttachmentDeleteInput};

async fn send(f: &Fixture, request: Request<Body>) -> (StatusCode, http::HeaderMap, Vec<u8>) {
    let response = f.assets.routes().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, headers, body)
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

async fn issue(f: &Fixture, resource: serde_json::Value) -> String {
    let resource: AssetResource = serde_json::from_value(resource).unwrap();
    f.assets.access.issue_asset_url(IssueAssetUrlInput::new(resource)).await.unwrap().relative_url
}

async fn upload_url(f: &Fixture, input: serde_json::Value) -> (String, String) {
    let input: AttachmentCreateUploadUrlInput = serde_json::from_value(input).unwrap();
    let issued = create_upload_url(&f.assets, input).await.unwrap();
    (issued.attachment_id, issued.relative_url)
}

fn post(uri: &str, body: Vec<u8>, content_length: Option<&str>) -> Request<Body> {
    let mut builder = Request::post(uri).header("content-type", "application/octet-stream");
    if let Some(length) = content_length {
        builder = builder.header("content-length", length);
    }
    builder.body(Body::from(body)).unwrap()
}

#[tokio::test]
async fn uploads_image_bytes_through_a_signed_url() {
    let f = Fixture::new().await;
    let (id, url) = upload_url(&f, json!({"name": "screenshot.png", "mimeType": "image/png", "sizeBytes": 6})).await;
    let (status, _, body) = send(&f, post(&url, vec![1, 2, 3], Some("3"))).await;
    assert_eq!(
        (status, body.as_slice()),
        (StatusCode::BAD_REQUEST, b"Content-Length must match the upload size.".as_slice())
    );
    let (status, _, body) = send(&f, post(&url, vec![1, 2, 3], None)).await;
    assert_eq!(
        (status, String::from_utf8(body).unwrap()),
        (StatusCode::BAD_REQUEST, "Body was 3 bytes, expected 6.".to_owned())
    );
    let (status, _, _) = send(&f, post(&url, vec![1, 2, 3, 4, 5, 6], Some("6"))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let stored = f.attachments_dir.join(format!("{id}.png"));
    assert!(stored.exists());
    delete(&f.assets, AttachmentDeleteInput { attachment_id: id }).await.unwrap();
    assert!(!stored.exists());

    // A generic file, streamed in chunks, then downloaded with its name and type.
    let (id, url) = upload_url(&f, json!({"type": "file", "name": "report.pdf", "mimeType": "application/pdf", "sizeBytes": 6})).await;
    let chunks = futures::stream::iter([Ok::<_, std::io::Error>(vec![1u8, 2, 3]), Ok(vec![4, 5, 6])]);
    let request = Request::post(&url).body(Body::from_stream(chunks)).unwrap();
    let (status, _, _) = send(&f, request).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let stored = f.attachments_dir.join(format!("{id}.pdf"));
    assert_eq!(std::fs::read(&stored).unwrap(), vec![1, 2, 3, 4, 5, 6]);

    let download = issue(
        &f,
        json!({"_tag": "attachment", "attachmentId": id, "fileName": "report.pdf", "mimeType": "application/pdf"}),
    )
    .await;
    let (status, headers, body) = send(&f, get(&download)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-disposition"], "attachment; filename=\"report.pdf\"");
    assert_eq!(headers["content-type"], "application/pdf");
    assert_eq!(headers["content-security-policy"], "default-src 'none'; sandbox");
    assert_eq!(body, vec![1, 2, 3, 4, 5, 6]);

    // Old clients mint without name or mime and still get a download.
    let bare = issue(&f, json!({"_tag": "attachment", "attachmentId": id})).await;
    let (status, headers, _) = send(&f, get(&bare)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-disposition"], "attachment");
    assert_eq!(headers["content-type"], "application/octet-stream");

    delete(&f.assets, AttachmentDeleteInput { attachment_id: id }).await.unwrap();
    assert!(!stored.exists());
}

#[tokio::test]
async fn rejects_unknown_tokens_and_bad_upload_payloads() {
    let f = Fixture::new().await;
    for uri in ["/api/assets/garbage/x.png", "/api/assets/x", "/api/assets//x.png"] {
        let (status, _, body) = send(&f, get(uri)).await;
        assert_eq!((status, body.as_slice()), (StatusCode::NOT_FOUND, b"Not Found".as_slice()), "{uri}");
    }
    let (status, _, _) = send(&f, post("/api/attachments/upload/garbage.sig", vec![1], None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Schema limits of attachments.createUploadUrl.
    for input in [
        json!({"name": "  ", "mimeType": "image/png", "sizeBytes": 6}),
        json!({"name": "a.png", "mimeType": "image/png", "sizeBytes": 0}),
        json!({"name": "a.png", "mimeType": "image/png", "sizeBytes": 10 * 1024 * 1024 + 1}),
        json!({"type": "file", "name": "a.bin", "mimeType": "application/octet-stream", "sizeBytes": 50 * 1024 * 1024 + 1}),
    ] {
        let decoded: AttachmentCreateUploadUrlInput = serde_json::from_value(input.clone()).unwrap();
        assert!(create_upload_url(&f.assets, decoded).await.is_err(), "{input}");
    }
}

#[tokio::test]
async fn rejects_an_over_limit_chunked_upload_over_a_real_socket_without_hanging() {
    let f = Fixture::new().await;
    let (_, url) = upload_url(
        &f,
        json!({"type": "file", "name": "big.bin", "mimeType": "application/octet-stream", "sizeBytes": 6}),
    )
    .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = f.assets.routes();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
    let head = format!("POST {url} HTTP/1.1\r\nhost: {address}\r\ncontent-type: application/octet-stream\r\ntransfer-encoding: chunked\r\n\r\n");
    socket.write_all(head.as_bytes()).await.unwrap();
    socket.write_all(b"4\r\n\0\0\0\0\r\n").await.unwrap();
    socket.write_all(b"4\r\n\0\0\0\0\r\n").await.unwrap();
    let mut response = vec![0u8; 1024];
    let read = tokio::time::timeout(std::time::Duration::from_secs(10), socket.read(&mut response))
        .await
        .expect("an answer")
        .unwrap();
    let text = String::from_utf8_lossy(&response[..read]);
    assert!(text.starts_with("HTTP/1.1 400"), "{text}");
    drop(socket);
    assert!(std::fs::read_dir(&f.attachments_dir).unwrap().next().is_none());
}

#[tokio::test]
async fn serves_host_media_with_ranges_and_head() {
    let f = Fixture::new().await;
    let media = f.mkdir("host-media");
    write(format!("{media}/screenshot.png"), "host media bytes");
    write(format!("{media}/recording.mp4"), "0123456789");

    let image = issue(
        &f,
        json!({"_tag": "media-file", "threadId": "elsewhere", "path": format!("{media}/screenshot.png")}),
    )
    .await;
    let (status, headers, body) = send(&f, get(&image)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(headers["cache-control"], "private, max-age=3600");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert!(headers["etag"].to_str().unwrap().starts_with("W/\""));
    assert!(headers.contains_key("last-modified"));
    assert_eq!(body, b"host media bytes");

    let video = issue(
        &f,
        json!({"_tag": "media-file", "threadId": "elsewhere", "path": format!("{media}/recording.mp4")}),
    )
    .await;
    let (status, headers, body) = send(&f, get(&video)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "video/mp4");
    assert_eq!(headers["accept-ranges"], "bytes");
    assert_eq!(headers["cache-control"], "private, no-store");
    assert!(!headers.contains_key("etag"));
    assert_eq!(body, b"0123456789");

    let ranged = Request::get(&video).header("range", "bytes=2-5").body(Body::empty()).unwrap();
    let (status, headers, body) = send(&f, ranged).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers["content-range"], "bytes 2-5/10");
    assert_eq!(headers["content-length"], "4");
    assert_eq!(body, b"2345");

    let suffix = Request::get(&video).header("range", "bytes=-3").body(Body::empty()).unwrap();
    assert_eq!(send(&f, suffix).await.2, b"789");

    let unsatisfiable = Request::get(&video).header("range", "bytes=10-").body(Body::empty()).unwrap();
    let (status, headers, _) = send(&f, unsatisfiable).await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(headers["content-range"], "bytes */10");

    // If-Range without a validator we can honour: the full file.
    let conditional = Request::get(&video)
        .header("range", "bytes=2-5")
        .header("if-range", "W/\"x\"")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&f, conditional).await.0, StatusCode::OK);

    let head = Request::builder()
        .method(Method::HEAD)
        .uri(&video)
        .header("range", "bytes=2-5")
        .body(Body::empty())
        .unwrap();
    let (status, headers, body) = send(&f, head).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-length"], "10");
    assert!(body.is_empty());
}

#[tokio::test]
async fn sandboxes_html_and_svg_previews() {
    let f = Fixture::new().await;
    let root = f.mkdir("draft");
    write(format!("{root}/note.html"), "<p>draft</p>");
    write(format!("{root}/icon.svg"), "<svg/>");
    let html = issue(&f, json!({"_tag": "draft-workspace-file", "cwd": root, "path": "note.html"})).await;
    let (status, headers, body) = send(&f, get(&html)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");
    assert_eq!(
        headers["content-security-policy"],
        "sandbox allow-scripts allow-forms allow-popups allow-modals"
    );
    assert_eq!(body, b"<p>draft</p>");

    let svg = issue(&f, json!({"_tag": "draft-workspace-file", "cwd": root, "path": "icon.svg"})).await;
    let (status, headers, _) = send(&f, get(&svg)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/svg+xml");
    assert_eq!(headers["content-security-policy"], "default-src 'none'; style-src 'unsafe-inline'; sandbox");
}

#[tokio::test]
async fn serves_project_favicons() {
    let f = Fixture::new().await;
    let project = f.mkdir("project");
    write(format!("{project}/public/favicon.svg"), "<svg>icon</svg>");
    let url = issue(&f, json!({"_tag": "project-favicon", "cwd": project})).await;
    let (status, headers, body) = send(&f, get(&url)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/svg+xml");
    assert_eq!(body, b"<svg>icon</svg>");
}
