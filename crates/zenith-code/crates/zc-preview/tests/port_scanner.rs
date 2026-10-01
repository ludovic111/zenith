//! Port of `PortScanner.test.ts`. The listener source and the HTTP probe are faked where the
//! TS test fakes `lsof` and `fetch`; the integration cases run against real sockets with the
//! real OS listener table and the real HTTP probe.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zc_preview::port_scanner::{
    classify_probe_response, HttpWebProbe, ListenerSource, RawListener, SystemListeners, WebProbe, CONFIGURED_LOCAL_SERVER_URLS_MAX_ITEMS,
    PREVIEW_URL_MAX_LENGTH,
};
use zc_preview::PortDiscovery;

const LSOF_TEST_PORT: u16 = 43_123;

/// `lsof` printing one node process on `*:43123`.
struct OneListener {
    pid: Arc<AtomicU32>,
}

impl ListenerSource for OneListener {
    fn listening_tcp(&self) -> Result<Vec<RawListener>, String> {
        Ok(vec![RawListener {
            ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: LSOF_TEST_PORT,
            pid: Some(self.pid.load(Ordering::SeqCst)),
            process_name: Some("node".into()),
        }])
    }
}

/// No listener table and nothing listening on the common ports.
struct NoListeners;

impl ListenerSource for NoListeners {
    fn listening_tcp(&self) -> Result<Vec<RawListener>, String> {
        Err("not installed".into())
    }
    fn is_listening_on_loopback(&self, _port: u16) -> bool {
        false
    }
}

/// A fake `fetch`: records every probed URL (as `URL.href`) and answers with
/// `(status, location, content type)`, or fails.
type Reply = (u16, Option<&'static str>, Option<&'static str>);
type Responder = Box<dyn Fn(&str) -> Option<Reply> + Send + Sync>;

struct FakeProbe {
    requests: Arc<Mutex<Vec<String>>>,
    respond: Responder,
}

#[async_trait]
impl WebProbe for FakeProbe {
    async fn is_web(&self, url: &str) -> bool {
        let href = url::Url::parse(url).map(|url| url.to_string()).unwrap_or_else(|_| url.to_owned());
        self.requests.lock().unwrap().push(href.clone());
        match (self.respond)(&href) {
            Some((status, location, content_type)) => classify_probe_response(status, location, content_type),
            None => false,
        }
    }
}

struct Harness {
    scanner: PortDiscovery,
    requests: Arc<Mutex<Vec<String>>>,
    clock: Arc<AtomicI64>,
    pid: Arc<AtomicU32>,
}

fn harness(source: Option<Arc<dyn ListenerSource>>, respond: Responder) -> Harness {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let clock = Arc::new(AtomicI64::new(1_000_000));
    let pid = Arc::new(AtomicU32::new(1234));
    let source = source.unwrap_or_else(|| Arc::new(OneListener { pid: pid.clone() }));
    let now = clock.clone();
    let scanner = PortDiscovery::with(
        source,
        Arc::new(FakeProbe {
            requests: requests.clone(),
            respond,
        }),
        Arc::new(move || now.load(Ordering::SeqCst)),
    );
    Harness { scanner, requests, clock, pid }
}

const HTML: Option<&str> = Some("text/html");

fn requests(harness: &Harness) -> Vec<String> {
    harness.requests.lock().unwrap().clone()
}

fn advance(harness: &Harness, millis: i64) {
    harness.clock.fetch_add(millis, Ordering::SeqCst);
}

#[tokio::test]
async fn revalidates_a_successful_html_probe_after_its_cache_entry_expires() {
    let responds = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let flag = responds.clone();
    let h = harness(None, Box::new(move |_| flag.load(Ordering::SeqCst).then_some((200, None, HTML))));
    assert_eq!(h.scanner.scan(&[]).await.len(), 1);
    assert_eq!(h.scanner.scan(&[]).await.len(), 1);
    assert_eq!(requests(&h), vec![format!("http://localhost:{LSOF_TEST_PORT}/")]);
    responds.store(false, Ordering::SeqCst);
    advance(&h, 15_000);
    assert_eq!(h.scanner.scan(&[]).await.len(), 0);
    assert_eq!(
        requests(&h),
        vec![
            format!("http://localhost:{LSOF_TEST_PORT}/"),
            format!("http://localhost:{LSOF_TEST_PORT}/"),
            format!("https://localhost:{LSOF_TEST_PORT}/"),
        ]
    );
}

#[tokio::test]
async fn keeps_a_full_configured_url_when_the_discovered_server_root_fails() {
    let configured = format!("http://localhost:{LSOF_TEST_PORT}/docs");
    let target = configured.clone();
    let h = harness(
        None,
        Box::new(move |url| Some(if url == target { (200, None, HTML) } else { (404, None, HTML) })),
    );
    let servers = h.scanner.scan(std::slice::from_ref(&configured)).await;
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].url, configured);
    assert!(requests(&h).contains(&configured));
}

#[tokio::test]
async fn probes_configured_custom_ports_through_a_canonical_loopback_host() {
    let h = harness(Some(Arc::new(NoListeners)), Box::new(|_| Some((200, None, HTML))));
    let servers = h.scanner.scan(&["http://0.0.0.0:43124/docs".to_owned()]).await;
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].host, "localhost");
    assert_eq!(servers[0].port, 43_124);
    assert_eq!(servers[0].url, "http://localhost:43124/docs");
    assert_eq!(requests(&h), vec!["http://localhost:43124/docs".to_owned()]);
}

#[tokio::test]
async fn preserves_explicit_loopback_hosts_and_bounds_wildcard_rewrites() {
    let ipv4 = "https://127.0.0.1:43125/docs".to_owned();
    let ipv6 = "http://[::1]:43126/docs".to_owned();
    let prefix = "http://0.0.0.0/";
    let maximum_wildcard = format!("{prefix}{}", "a".repeat(PREVIEW_URL_MAX_LENGTH - prefix.len()));
    let h = harness(Some(Arc::new(NoListeners)), Box::new(|_| Some((200, None, HTML))));
    let servers = h.scanner.scan(&[ipv4.clone(), ipv6.clone(), maximum_wildcard]).await;
    assert_eq!(
        servers.iter().map(|server| server.url.clone()).collect::<Vec<_>>(),
        vec![ipv4.clone(), ipv6.clone()]
    );
    assert_eq!(requests(&h), vec![ipv4, ipv6]);
}

#[tokio::test]
async fn projects_configured_paths_independently_for_simultaneous_subscribers() {
    let docs = format!("http://localhost:{LSOF_TEST_PORT}/docs");
    let admin = format!("http://localhost:{LSOF_TEST_PORT}/admin");
    let (d, a) = (docs.clone(), admin.clone());
    let h = harness(
        None,
        Box::new(move |url| Some(if url == d || url == a { (200, None, HTML) } else { (404, None, None) })),
    );
    let (_docs_guard, mut docs_changes) = h.scanner.subscribe(std::slice::from_ref(&docs), Vec::new());
    let (_admin_guard, mut admin_changes) = h.scanner.subscribe(std::slice::from_ref(&admin), Vec::new());
    let _retain = h.scanner.retain().await;
    assert_eq!(docs_changes.try_recv().unwrap()[0].url, docs);
    assert_eq!(admin_changes.try_recv().unwrap()[0].url, admin);
}

#[tokio::test]
async fn keeps_each_subscribers_candidates_when_their_combined_union_exceeds_the_per_client_cap() {
    let first: Vec<String> = (0..CONFIGURED_LOCAL_SERVER_URLS_MAX_ITEMS)
        .map(|i| format!("http://localhost:{LSOF_TEST_PORT}/app-{i}"))
        .collect();
    let second = format!("http://localhost:{LSOF_TEST_PORT}/app-{CONFIGURED_LOCAL_SERVER_URLS_MAX_ITEMS}");
    let target = second.clone();
    let h = harness(
        None,
        Box::new(move |url| Some(if url == target { (200, None, HTML) } else { (404, None, None) })),
    );
    let (_first_guard, _first_changes) = h.scanner.subscribe(&first, Vec::new());
    let (_second_guard, mut second_changes) = h.scanner.subscribe(std::slice::from_ref(&second), Vec::new());
    let _retain = h.scanner.retain().await;
    assert_eq!(second_changes.try_recv().unwrap()[0].url, second);
}

#[tokio::test]
async fn stops_probing_a_subscribers_configured_paths_after_it_ends() {
    let docs = format!("http://localhost:{LSOF_TEST_PORT}/docs");
    let admin = format!("http://localhost:{LSOF_TEST_PORT}/admin");
    let (d, a) = (docs.clone(), admin.clone());
    let h = harness(
        None,
        Box::new(move |url| Some(if url == d || url == a { (200, None, HTML) } else { (404, None, None) })),
    );
    let (docs_guard, _docs_changes) = h.scanner.subscribe(std::slice::from_ref(&docs), Vec::new());
    let (_admin_guard, _admin_changes) = h.scanner.subscribe(std::slice::from_ref(&admin), Vec::new());
    let _retain = h.scanner.retain().await;
    drop(docs_guard);
    h.requests.lock().unwrap().clear();
    advance(&h, 15_000);
    h.scanner.poll_tick().await;
    assert!(requests(&h).contains(&admin));
    assert!(!requests(&h).contains(&docs));
}

#[tokio::test(start_paused = true)]
async fn polls_only_while_a_client_retains_the_scanner() {
    let h = harness(None, Box::new(|_| Some((200, None, HTML))));
    let task = h.scanner.start();
    tokio::time::sleep(Duration::from_secs(15)).await;
    assert!(requests(&h).is_empty());
    let retain = h.scanner.retain().await;
    assert_eq!(requests(&h).len(), 1);
    // Polls every 3 s while retained (the cached probe answers), then stops.
    advance(&h, 15_000);
    tokio::time::sleep(Duration::from_millis(3_100)).await;
    assert_eq!(requests(&h).len(), 2);
    drop(retain);
    advance(&h, 15_000);
    tokio::time::sleep(Duration::from_secs(9)).await;
    assert_eq!(requests(&h).len(), 2);
    task.abort();
}

#[tokio::test]
async fn uses_the_current_configured_fragment_when_readiness_comes_from_cache() {
    let h = harness(None, Box::new(|_| Some((200, None, HTML))));
    let old = format!("http://localhost:{LSOF_TEST_PORT}/docs#old");
    let new = format!("http://localhost:{LSOF_TEST_PORT}/docs#new");
    assert_eq!(h.scanner.scan(std::slice::from_ref(&old)).await[0].url, old);
    let count = requests(&h).len();
    assert_eq!(h.scanner.scan(std::slice::from_ref(&new)).await[0].url, new);
    assert_eq!(requests(&h).len(), count);
}

#[tokio::test]
async fn shares_a_configured_root_probe_with_discovered_root_classification() {
    let h = harness(None, Box::new(|_| Some((200, None, HTML))));
    let root = format!("http://localhost:{LSOF_TEST_PORT}/");
    assert_eq!(h.scanner.scan(std::slice::from_ref(&root)).await.len(), 1);
    assert_eq!(requests(&h), vec![root.clone()]);
    advance(&h, 15_000);
    assert_eq!(h.scanner.scan(std::slice::from_ref(&root)).await.len(), 1);
    assert_eq!(requests(&h), vec![root.clone(), root]);
}

#[tokio::test]
async fn caches_a_failed_web_probe_until_its_bounded_cache_entry_expires() {
    let responds = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = responds.clone();
    let h = harness(None, Box::new(move |_| flag.load(Ordering::SeqCst).then_some((200, None, HTML))));
    assert_eq!(h.scanner.scan(&[]).await.len(), 0);
    assert_eq!(h.scanner.scan(&[]).await.len(), 0);
    assert_eq!(requests(&h).len(), 2);
    responds.store(true, Ordering::SeqCst);
    advance(&h, 15_000);
    assert_eq!(h.scanner.scan(&[]).await.len(), 1);
    assert_eq!(requests(&h).len(), 3);
}

#[tokio::test]
async fn falls_back_to_https() {
    let h = harness(
        None,
        Box::new(|url| {
            if url.starts_with("http:") {
                None
            } else {
                Some((302, Some("https://example.com"), None))
            }
        }),
    );
    let servers = h.scanner.scan(&[]).await;
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].url, format!("https://localhost:{LSOF_TEST_PORT}"));
    assert_eq!(servers[0].pid, Some(1234));
    assert_eq!(servers[0].process_name.as_deref(), Some("node"));
}

#[tokio::test]
async fn excludes_http_errors_non_navigation_responses_and_successful_non_documents() {
    let response: Arc<Mutex<Reply>> = Arc::new(Mutex::new((404, None, HTML)));
    let current = response.clone();
    let h = harness(None, Box::new(move |_| Some(*current.lock().unwrap())));
    assert!(h.scanner.scan(&[]).await.is_empty());
    for (next, expected) in [
        ((200, None, Some("application/json")), 0),
        ((200, None, Some("text/plain")), 0),
        ((304, Some("/cached"), None), 0),
        ((204, None, HTML), 0),
        ((302, None, None), 0),
        ((200, None, Some("application/xhtml+xml; charset=utf-8")), 1),
    ] {
        // A new listener pid invalidates the cached classification.
        h.pid.fetch_add(1, Ordering::SeqCst);
        *response.lock().unwrap() = next;
        assert_eq!(h.scanner.scan(&[]).await.len(), expected, "{next:?}");
    }
}

#[tokio::test]
async fn tags_servers_with_the_terminal_that_started_them() {
    let h = harness(None, Box::new(|_| Some((200, None, HTML))));
    h.scanner.register_terminal_processes("thread-1", "default", &[99, 1234]);
    let servers = h.scanner.scan(&[]).await;
    let terminal = servers[0].terminal.clone().unwrap();
    assert_eq!((terminal.thread_id.as_str(), terminal.terminal_id.as_str()), ("thread-1", "default"));
    h.scanner.unregister_terminal("thread-1", "default");
    advance(&h, 15_000);
    assert_eq!(h.scanner.scan(&[]).await[0].terminal, None);
}

// The real HTTP probe.

async fn serve_once(response: &'static [u8]) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                let mut buffer = [0u8; 1024];
                let _ = socket.read(&mut buffer).await;
                if !response.is_empty() {
                    let _ = socket.write_all(response).await;
                    let _ = socket.shutdown().await;
                } else {
                    // Never answer.
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            });
        }
    });
    (port, task)
}

#[tokio::test]
async fn the_http_probe_does_not_follow_redirects_and_gives_up_after_its_timeout() {
    let probe = HttpWebProbe::default();
    let (redirect, _r) = serve_once(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/elsewhere\r\nContent-Length: 0\r\n\r\n").await;
    assert!(probe.is_web(&format!("http://127.0.0.1:{redirect}")).await);
    let (html, _h) = serve_once(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 5\r\n\r\nhello").await;
    assert!(probe.is_web(&format!("http://127.0.0.1:{html}")).await);
    let (silent, _s) = serve_once(b"").await;
    let started = std::time::Instant::now();
    assert!(!probe.is_web(&format!("http://127.0.0.1:{silent}")).await);
    assert!(started.elapsed() < Duration::from_secs(3));
}

// PortDiscovery integration: real sockets, the OS listener table, the real probe.

#[tokio::test]
async fn scan_returns_an_http_server_we_just_opened_and_excludes_one_that_does_not_speak_http() {
    let (web, _w) = serve_once(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 5\r\n\r\nhello").await;
    let (mysql, _m) = serve_once(b"MYSQL\r\n\r\n").await;
    let scanner = PortDiscovery::with(Arc::new(SystemListeners), Arc::new(HttpWebProbe::default()), Arc::new(zc_core_now));
    let servers = scanner.scan(&[]).await;
    let found = servers
        .iter()
        .find(|server| server.port == web)
        .unwrap_or_else(|| panic!("{web} not in {servers:?}"));
    assert_eq!(found.host, "localhost");
    assert_eq!(found.url, format!("http://localhost:{web}"));
    assert_eq!(found.pid, Some(std::process::id()));
    assert!(!servers.iter().any(|server| server.port == mysql));
}

#[tokio::test]
async fn retain_drives_an_immediate_broadcast_to_subscribers() {
    let (web, _w) = serve_once(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 5\r\n\r\nhello").await;
    let scanner = PortDiscovery::with(Arc::new(SystemListeners), Arc::new(HttpWebProbe::default()), Arc::new(zc_core_now));
    let (_guard, mut changes) = scanner.subscribe(&[], Vec::new());
    let _retain = scanner.retain().await;
    let servers = changes.try_recv().unwrap();
    assert!(servers.iter().any(|server| server.port == web));
}

fn zc_core_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}
