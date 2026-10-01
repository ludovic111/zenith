//! HTTP helpers shared by the Bitbucket API and the `fj` API path: a bounded body reader
//! (`collectUint8StreamText`) and the client both use (redirects are never followed by the
//! client; callers decide).

use std::time::Duration;

/// `CollectedUint8StreamText`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CollectedText {
    pub text: String,
    pub truncated: bool,
    pub bytes: usize,
    pub invalid_utf8: bool,
}

/// Reads a response body, keeping at most `max_bytes`.
pub async fn collect_body(mut response: reqwest::Response, max_bytes: usize) -> Result<CollectedText, reqwest::Error> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = response.chunk().await? {
        let remaining = max_bytes.saturating_sub(buffer.len());
        if chunk.len() > remaining {
            buffer.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        buffer.extend_from_slice(&chunk);
    }
    let bytes = buffer.len();
    let (text, invalid_utf8) = match String::from_utf8(buffer) {
        Ok(text) => (text, false),
        Err(error) => (String::from_utf8_lossy(error.as_bytes()).into_owned(), true),
    };
    Ok(CollectedText {
        text,
        truncated,
        bytes,
        invalid_utf8,
    })
}

/// A client that never follows redirects on its own.
pub fn client(timeout: Option<Duration>) -> reqwest::Client {
    let mut builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }
    builder.build().expect("an HTTP client with default TLS settings")
}
