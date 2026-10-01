//! A local mock HTTP/1.1 server: records every request and answers from a closure.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One recorded request.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    pub target: String,
    pub headers: HashMap<String, String>,
    pub body: String,
}

/// What the handler answers: status, headers, body, and an optional lie about the length (the
/// connection then closes early, failing the body read).
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub declared_length: Option<usize>,
}

pub fn reply(status: u16, body: impl Into<String>) -> Reply {
    Reply {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        body: body.into(),
        declared_length: None,
    }
}

pub type Handler = Arc<dyn Fn(&Seen) -> Reply + Send + Sync>;

pub struct MockServer {
    pub base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl MockServer {
    pub async fn start(handler: impl Fn(&Seen) -> Reply + Send + Sync + 'static) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let handler: Handler = Arc::new(handler);
        let record = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let handler = handler.clone();
                let record = record.clone();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let header_end = loop {
                        let n = socket.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..n]);
                        if let Some(index) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                            break index + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
                    let mut lines = head.split("\r\n");
                    let request_line = lines.next().unwrap_or_default().to_owned();
                    let mut parts = request_line.split(' ');
                    let method = parts.next().unwrap_or_default().to_owned();
                    let target = parts.next().unwrap_or_default().to_owned();
                    let headers: HashMap<String, String> = lines
                        .filter_map(|line| line.split_once(':').map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_owned())))
                        .collect();
                    let length: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
                    while buffer.len() < header_end + length {
                        let n = socket.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buffer.extend_from_slice(&chunk[..n]);
                    }
                    let seen = Seen {
                        method,
                        target,
                        headers,
                        body: String::from_utf8_lossy(&buffer[header_end..]).into_owned(),
                    };
                    let answer = handler(&seen);
                    record.lock().unwrap().push(seen);
                    let mut response = format!(
                        "HTTP/1.1 {} X\r\nconnection: close\r\ncontent-length: {}\r\n",
                        answer.status,
                        answer.declared_length.unwrap_or(answer.body.len())
                    );
                    for (key, value) in &answer.headers {
                        response.push_str(&format!("{key}: {value}\r\n"));
                    }
                    response.push_str("\r\n");
                    response.push_str(&answer.body);
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self {
            base: format!("http://127.0.0.1:{port}/2.0"),
            seen,
        }
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}
