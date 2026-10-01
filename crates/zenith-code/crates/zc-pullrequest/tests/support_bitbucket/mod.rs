//! Shared by the `bitbucket_*` tests: a scripted [`BitbucketRequester`] (the TS tests'
//! `vi.fn()` mock of `BitbucketApi.request`) and a local HTTP/1.1 stub standing in for the
//! Bitbucket API (never the real one).

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zc_pullrequest::bitbucket::BitbucketRequester;
use zc_sourcecontrol::bitbucket::api::{BitbucketRequest, BitbucketResponseBody};
use zc_sourcecontrol::bitbucket::BitbucketApiError;
use zc_sourcecontrol::errors::Cause;

pub type Answer = Result<BitbucketResponseBody, BitbucketApiError>;

/// `{ body, truncated: false }`.
pub fn response(body: impl Into<String>) -> Answer {
    Ok(BitbucketResponseBody {
        body: body.into(),
        truncated: false,
    })
}

/// A `BitbucketResponseError`.
pub fn response_error(status: u16, retry_at: Option<i64>) -> BitbucketApiError {
    BitbucketApiError::Response {
        operation: "request",
        status,
        response_body_length: 0,
        retry_at,
    }
}

/// A `BitbucketResponseBodyReadError`.
pub fn body_read_error(status: u16, retry_at: Option<i64>) -> BitbucketApiError {
    BitbucketApiError::ResponseBodyRead {
        operation: "request",
        status,
        retry_at,
        cause: Cause::message("response stream failed"),
    }
}

type Route = Box<dyn Fn(&BitbucketRequest) -> Answer + Send + Sync>;

/// Answers queued with [`MockRequester::once`] first, then the [`MockRequester::always`] answer,
/// then the route; records every call.
#[derive(Default)]
pub struct MockRequester {
    calls: Mutex<Vec<BitbucketRequest>>,
    once: Mutex<VecDeque<Answer>>,
    always: Mutex<Option<Answer>>,
    route: Mutex<Option<Route>>,
}

impl MockRequester {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// `mockReturnValueOnce`.
    pub fn once(&self, answer: Answer) -> &Self {
        self.once.lock().unwrap().push_back(answer);
        self
    }

    /// `mockReturnValue`.
    pub fn always(&self, answer: Answer) {
        *self.always.lock().unwrap() = Some(answer);
    }

    /// Answers by request.
    pub fn route(&self, route: impl Fn(&BitbucketRequest) -> Answer + Send + Sync + 'static) {
        *self.route.lock().unwrap() = Some(Box::new(route));
    }

    pub fn calls(&self) -> Vec<BitbucketRequest> {
        self.calls.lock().unwrap().clone()
    }

    /// The request the nth call made.
    pub fn call_at(&self, index: usize) -> BitbucketRequest {
        self.calls().get(index).cloned().unwrap_or_else(|| panic!("no call #{index}"))
    }

    /// The filter expression of the nth request, read back out of its query string.
    pub fn filter_of_call(&self, index: usize) -> Option<String> {
        let url = self.call_at(index).url;
        let query = url.split_once('?').map(|(_, query)| query).unwrap_or_default();
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(key, _)| key == "q")
            .map(|(_, value)| value.into_owned())
    }

    /// The JSON body of the nth request.
    pub fn body_of_call(&self, index: usize) -> serde_json::Value {
        serde_json::from_str(self.call_at(index).body.as_deref().unwrap_or_default()).unwrap()
    }
}

#[async_trait]
impl BitbucketRequester for MockRequester {
    async fn request(&self, input: BitbucketRequest) -> Answer {
        self.calls.lock().unwrap().push(input.clone());
        if let Some(answer) = self.once.lock().unwrap().pop_front() {
            return answer;
        }
        if let Some(answer) = self.always.lock().unwrap().clone() {
            return answer;
        }
        match &*self.route.lock().unwrap() {
            Some(route) => route(&input),
            None => panic!("no answer for {} {}", input.method, input.url),
        }
    }
}

/// One request the stub saw.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    pub target: String,
    pub headers: HashMap<String, String>,
    pub body: String,
}

/// What the stub answers.
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

pub fn reply(status: u16, body: impl Into<String>) -> Reply {
    Reply {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        body: body.into(),
    }
}

/// A local HTTP/1.1 server answering from a closure, recording every request.
pub struct MockServer {
    /// `http://127.0.0.1:<port>/2.0`.
    pub base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl MockServer {
    pub async fn start(handler: impl Fn(&Seen) -> Reply + Send + Sync + 'static) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let handler: Arc<dyn Fn(&Seen) -> Reply + Send + Sync> = Arc::new(handler);
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
                    let mut response = format!("HTTP/1.1 {} X\r\nconnection: close\r\ncontent-length: {}\r\n", answer.status, answer.body.len());
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
