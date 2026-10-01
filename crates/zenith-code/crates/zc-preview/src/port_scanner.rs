//! `preview/PortScanner.ts`: the local servers the preview panel recommends.
//!
//! Listening TCP sockets come from the OS without `lsof` (the `listeners` crate: libproc on
//! macOS, `/proc` on Linux, the IP helper API on Windows); when that fails, a curated list of
//! common dev ports is checked by trying to bind them. A port is published only after a
//! bounded HTTP(S) probe finds an HTML document or a redirect: positive and negative results
//! are cached for 15 s per URL and listener pid. Servers started from a zenith terminal carry
//! that terminal (its process ids are registered by the terminal manager).
//!
//! Polling is reference-counted: one background task ticks every 3 s but only scans while a
//! client [`PortDiscovery::retain`]s the scanner, and a first retainer gets an immediate scan.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::future::{FutureExt, Shared};
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use url::Url;

use crate::url::is_loopback_host;

/// `COMMON_DEV_PORTS`.
pub const COMMON_DEV_PORTS: [u16; 16] = [3000, 3001, 3333, 4173, 4200, 4321, 5000, 5173, 5174, 5175, 5500, 8000, 8080, 8081, 8888, 9000];
/// `CONFIGURED_LOCAL_SERVER_URLS_MAX_ITEMS`.
pub const CONFIGURED_LOCAL_SERVER_URLS_MAX_ITEMS: usize = 32;
/// `PREVIEW_URL_MAX_LENGTH`.
pub const PREVIEW_URL_MAX_LENGTH: usize = 2_048;
/// `POLL_INTERVAL`.
pub const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// `WEB_PROBE_TIMEOUT`.
pub const WEB_PROBE_TIMEOUT: Duration = Duration::from_secs(1);
/// `WEB_PROBE_CACHE_TTL_MS`.
pub const WEB_PROBE_CACHE_TTL_MS: i64 = 15_000;
const WEB_PROBE_CONCURRENCY: usize = 16;

/// The terminal a server was started from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TerminalOwner {
    pub thread_id: String,
    pub terminal_id: String,
}

/// `DiscoveredLocalServer`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredLocalServer {
    pub host: String,
    pub port: u16,
    pub url: String,
    pub process_name: Option<String>,
    pub pid: Option<u32>,
    pub terminal: Option<TerminalOwner>,
}

impl DiscoveredLocalServer {
    pub fn to_json(&self) -> Value {
        json!({
            "host": self.host,
            "port": self.port,
            "url": self.url,
            "processName": self.process_name,
            "pid": self.pid,
            "terminal": self.terminal.as_ref().map(|owner| json!({"threadId": owner.thread_id, "terminalId": owner.terminal_id})),
        })
    }
}

/// One listening TCP socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawListener {
    pub ip: IpAddr,
    pub port: u16,
    pub pid: Option<u32>,
    pub process_name: Option<String>,
}

/// Where listening sockets come from.
pub trait ListenerSource: Send + Sync {
    /// The listening TCP sockets, or an error (the scanner then checks the common ports).
    fn listening_tcp(&self) -> Result<Vec<RawListener>, String>;
    /// Whether something listens on `127.0.0.1:port` (the fallback check).
    fn is_listening_on_loopback(&self, port: u16) -> bool {
        std::net::TcpListener::bind(("127.0.0.1", port)).is_err()
    }
}

/// The OS listeners (`listeners::get_all`).
pub struct SystemListeners;

impl ListenerSource for SystemListeners {
    fn listening_tcp(&self) -> Result<Vec<RawListener>, String> {
        let all = listeners::get_all().map_err(|error| error.to_string())?;
        Ok(all
            .into_iter()
            .filter(|listener| listener.protocol == listeners::Protocol::TCP && listener.state == listeners::SocketState::Listen)
            .map(|listener| RawListener {
                ip: listener.socket.ip(),
                port: listener.socket.port(),
                pid: Some(listener.process.pid).filter(|pid| *pid > 0),
                process_name: Some(listener.process.name.trim().to_owned()).filter(|name| !name.is_empty()),
            })
            .collect())
    }
}

/// The HTTP probe: whether `url` serves a navigable document.
#[async_trait]
pub trait WebProbe: Send + Sync {
    async fn is_web(&self, url: &str) -> bool;
}

/// `probeWebUrl` over reqwest: a GET that follows no redirect, bounded by
/// [`WEB_PROBE_TIMEOUT`]; a redirect with a location, or a 2xx HTML/XHTML document (not 204/205),
/// is a web page.
pub struct HttpWebProbe {
    client: reqwest::Client,
}

impl Default for HttpWebProbe {
    fn default() -> Self {
        Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(WEB_PROBE_TIMEOUT)
                .build()
                .expect("an HTTP client"),
        }
    }
}

/// The verdict on one probe response.
pub fn classify_probe_response(status: u16, location: Option<&str>, content_type: Option<&str>) -> bool {
    if matches!(status, 301 | 302 | 303 | 307 | 308) && location.is_some_and(|location| !location.trim().is_empty()) {
        return true;
    }
    if !(200..300).contains(&status) || status == 204 || status == 205 {
        return false;
    }
    let content_type = content_type.and_then(|value| value.split(';').next()).map(|value| value.trim().to_lowercase());
    matches!(content_type.as_deref(), Some("text/html") | Some("application/xhtml+xml"))
}

#[async_trait]
impl WebProbe for HttpWebProbe {
    async fn is_web(&self, url: &str) -> bool {
        let request = self.client.get(url).send();
        match tokio::time::timeout(WEB_PROBE_TIMEOUT, request).await {
            Ok(Ok(response)) => {
                let header = |name: &str| response.headers().get(name).and_then(|value| value.to_str().ok());
                classify_probe_response(response.status().as_u16(), header("location"), header("content-type"))
            }
            _ => false,
        }
    }
}

/// Milliseconds since the epoch.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

#[derive(Debug, Clone, Copy)]
struct ProbeCacheEntry {
    pid: Option<u32>,
    is_web: bool,
    expires_at: i64,
}

struct Subscription {
    configured_urls: Vec<String>,
    last_snapshot: Vec<DiscoveredLocalServer>,
    sender: mpsc::UnboundedSender<Vec<DiscoveredLocalServer>>,
}

#[derive(Default)]
struct ScannerState {
    subscriptions: HashMap<u64, Subscription>,
    terminal_processes: HashMap<(String, String), (TerminalOwner, HashSet<u32>)>,
    retain_count: usize,
}

struct ProbeSnapshot {
    discovered: Vec<DiscoveredLocalServer>,
    /// By configured URL cache key.
    configured: HashMap<String, DiscoveredLocalServer>,
}

struct Inner {
    source: Arc<dyn ListenerSource>,
    probe: Arc<dyn WebProbe>,
    clock: Clock,
    state: Mutex<ScannerState>,
    cache: Mutex<HashMap<String, ProbeCacheEntry>>,
    scan_lock: tokio::sync::Mutex<()>,
    next_subscription: AtomicU64,
}

/// The scanner. Cheap to clone.
#[derive(Clone)]
pub struct PortDiscovery {
    inner: Arc<Inner>,
}

/// `parseConfiguredUrl`: an http(s) URL on a loopback host.
fn parse_configured_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw).ok()?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return None;
    }
    is_loopback_host(url.host_str().unwrap_or("")).then_some(url)
}

/// `localServerKey`: every loopback spelling is one host.
fn local_server_key(host: &str, port: u16) -> String {
    if is_loopback_host(host) {
        format!("loopback:{port}")
    } else {
        format!("{}:{port}", host.to_lowercase())
    }
}

/// `urlPort`.
fn url_port(url: &Url) -> u16 {
    url.port().unwrap_or(if url.scheme() == "http" { 80 } else { 443 })
}

/// `webProbeCacheKey`: the URL without its fragment.
fn web_probe_cache_key(raw: &str) -> String {
    match Url::parse(raw) {
        Ok(mut url) => {
            url.set_fragment(None);
            url.to_string()
        }
        Err(_) => raw.to_owned(),
    }
}

fn js_length(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `normalizeConfiguredUrls`: at most 32 loopback http(s) URLs of bounded length, `0.0.0.0`
/// rewritten to `localhost`, deduplicated in order.
pub fn normalize_configured_urls(urls: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    urls.iter()
        .take(CONFIGURED_LOCAL_SERVER_URLS_MAX_ITEMS)
        .filter(|raw| js_length(raw) <= PREVIEW_URL_MAX_LENGTH)
        .filter_map(|raw| parse_configured_url(raw))
        .filter(|url| js_length(url.as_str()) <= PREVIEW_URL_MAX_LENGTH)
        .map(|mut url| {
            if url.host_str() == Some("0.0.0.0") {
                let _ = url.set_host(Some("localhost"));
            }
            url.to_string()
        })
        .filter(|url| js_length(url) <= PREVIEW_URL_MAX_LENGTH)
        .filter(|url| seen.insert(url.clone()))
        .collect()
}

/// `projectWebProbeSnapshot`: one subscriber's view (its configured URLs first, then the
/// discovered roots), by port.
fn project_snapshot(snapshot: &ProbeSnapshot, configured_urls: &[String]) -> Vec<DiscoveredLocalServer> {
    let mut visible: Vec<(String, DiscoveredLocalServer)> = Vec::new();
    for raw in normalize_configured_urls(configured_urls) {
        let Ok(url) = Url::parse(&raw) else { continue };
        let key = local_server_key(url.host_str().unwrap_or(""), url_port(&url));
        if visible.iter().any(|(existing, _)| *existing == key) {
            continue;
        }
        if let Some(configured) = snapshot.configured.get(&web_probe_cache_key(&raw)) {
            visible.push((
                key,
                DiscoveredLocalServer {
                    url: raw.clone(),
                    ..configured.clone()
                },
            ));
        }
    }
    for server in &snapshot.discovered {
        let key = local_server_key(&server.host, server.port);
        if !visible.iter().any(|(existing, _)| *existing == key) {
            visible.push((key, server.clone()));
        }
    }
    let mut servers: Vec<DiscoveredLocalServer> = visible.into_iter().map(|(_, server)| server).collect();
    servers.sort_by_key(|server| server.port);
    servers
}

/// Whether a socket address is local (`LSOF_LOCAL_HOST_TOKENS`: wildcard or loopback).
fn is_local_listener(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_unspecified() || *ip == std::net::Ipv4Addr::LOCALHOST,
        IpAddr::V6(ip) => ip.is_unspecified() || ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4 == std::net::Ipv4Addr::LOCALHOST),
    }
}

/// `parseLsofOutput` on listeners: local TCP listeners, one per port (the lowest pid wins),
/// with their terminal.
pub fn local_servers_from_listeners(listeners: &[RawListener], terminals: &HashMap<u32, TerminalOwner>) -> Vec<DiscoveredLocalServer> {
    let mut sorted: Vec<&RawListener> = listeners
        .iter()
        .filter(|listener| is_local_listener(&listener.ip) && listener.port > 0)
        .collect();
    sorted.sort_by_key(|listener| (listener.port, listener.pid.unwrap_or(u32::MAX)));
    let mut servers: Vec<DiscoveredLocalServer> = Vec::new();
    for listener in sorted {
        if servers.iter().any(|server| server.port == listener.port) {
            continue;
        }
        servers.push(DiscoveredLocalServer {
            host: "localhost".into(),
            port: listener.port,
            url: format!("http://localhost:{}", listener.port),
            process_name: listener.process_name.clone(),
            pid: listener.pid,
            terminal: listener.pid.and_then(|pid| terminals.get(&pid).cloned()),
        });
    }
    servers
}

type ProbeFuture = Shared<Pin<Box<dyn Future<Output = (bool, bool)> + Send>>>;

/// Per probed URL of a group: its cache key, whether it is a web page, whether it was probed
/// now (not answered from the cache).
type GroupProbes = Vec<(String, bool, bool)>;

/// Keeps a subscriber registered until dropped.
pub struct SubscriptionGuard {
    inner: Arc<Inner>,
    id: u64,
}

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        self.inner.state.lock().unwrap().subscriptions.remove(&self.id);
    }
}

/// Keeps the scanner polling until dropped.
pub struct RetainGuard {
    inner: Arc<Inner>,
}

impl Drop for RetainGuard {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().unwrap();
        state.retain_count = state.retain_count.saturating_sub(1);
    }
}

impl PortDiscovery {
    /// The scanner over the OS listeners and a real HTTP probe.
    pub fn new() -> Self {
        Self::with(
            Arc::new(SystemListeners),
            Arc::new(HttpWebProbe::default()),
            Arc::new(zc_core::time::now_millis),
        )
    }

    pub fn with(source: Arc<dyn ListenerSource>, probe: Arc<dyn WebProbe>, clock: Clock) -> Self {
        Self {
            inner: Arc::new(Inner {
                source,
                probe,
                clock,
                state: Mutex::new(ScannerState::default()),
                cache: Mutex::new(HashMap::new()),
                scan_lock: tokio::sync::Mutex::new(()),
                next_subscription: AtomicU64::new(0),
            }),
        }
    }

    /// The polling task: every [`POLL_INTERVAL`], a scan and broadcast while retained.
    pub fn start(&self) -> tokio::task::JoinHandle<()> {
        let scanner = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + POLL_INTERVAL, POLL_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let retained = scanner.inner.state.lock().unwrap().retain_count > 0;
                if retained {
                    scanner.poll_tick().await;
                }
            }
        })
    }

    fn terminal_by_pid(&self) -> HashMap<u32, TerminalOwner> {
        let state = self.inner.state.lock().unwrap();
        let mut by_pid = HashMap::new();
        for (owner, pids) in state.terminal_processes.values() {
            for pid in pids {
                by_pid.insert(*pid, owner.clone());
            }
        }
        by_pid
    }

    async fn listening_servers(&self) -> Vec<DiscoveredLocalServer> {
        let terminals = self.terminal_by_pid();
        let source = self.inner.source.clone();
        let listed = tokio::task::spawn_blocking(move || source.listening_tcp())
            .await
            .unwrap_or_else(|error| Err(error.to_string()));
        match listed {
            Ok(listeners) => local_servers_from_listeners(&listeners, &terminals),
            Err(error) => {
                tracing::debug!(%error, "preview port listener probe failed; falling back to common-port probes");
                let source = self.inner.source.clone();
                let listening = tokio::task::spawn_blocking(move || {
                    COMMON_DEV_PORTS
                        .iter()
                        .copied()
                        .filter(|port| source.is_listening_on_loopback(*port))
                        .collect::<Vec<_>>()
                })
                .await
                .unwrap_or_default();
                listening
                    .into_iter()
                    .map(|port| DiscoveredLocalServer {
                        host: "localhost".into(),
                        port,
                        url: format!("http://localhost:{port}"),
                        process_name: None,
                        pid: None,
                        terminal: None,
                    })
                    .collect()
            }
        }
    }

    /// `probeWebServers`: configured URLs and discovered roots, each probed once per batch
    /// (per URL and pid), with the cache.
    async fn probe_servers(&self, servers: Vec<DiscoveredLocalServer>, configured_urls: &[String]) -> ProbeSnapshot {
        let now = (self.inner.clock)();
        let cached = self.inner.cache.lock().unwrap().clone();
        struct Group {
            server: DiscoveredLocalServer,
            urls: Vec<String>,
            configured_key: Option<String>,
        }
        let mut groups: Vec<Group> = Vec::new();
        let mut configured_resources = HashSet::new();
        for raw in configured_urls {
            let Ok(url) = Url::parse(raw) else { continue };
            let port = url_port(&url);
            let host = url.host_str().unwrap_or("").to_owned();
            let key = local_server_key(&host, port);
            let resource = web_probe_cache_key(raw);
            if !configured_resources.insert(resource.clone()) {
                continue;
            }
            let server = servers
                .iter()
                .find(|server| local_server_key(&server.host, server.port) == key)
                .cloned()
                .unwrap_or(DiscoveredLocalServer {
                    host,
                    port,
                    url: raw.clone(),
                    process_name: None,
                    pid: None,
                    terminal: None,
                });
            groups.push(Group {
                server,
                urls: vec![raw.clone()],
                configured_key: Some(resource),
            });
        }
        for server in &servers {
            groups.push(Group {
                server: server.clone(),
                urls: vec![
                    format!("http://{}:{}", server.host, server.port),
                    format!("https://{}:{}", server.host, server.port),
                ],
                configured_key: None,
            });
        }

        // One probe per (URL, pid) in this batch; `bool`s are (is web, freshly probed).
        let batch: Arc<Mutex<HashMap<String, ProbeFuture>>> = Arc::new(Mutex::new(HashMap::new()));
        let probe_of = |url: &str, pid: Option<u32>| -> ProbeFuture {
            let key = web_probe_cache_key(url);
            let identity = format!("{key}\u{0}{}", pid.map(|pid| pid.to_string()).unwrap_or_default());
            let mut batch = batch.lock().unwrap();
            if let Some(existing) = batch.get(&identity) {
                return existing.clone();
            }
            let current = cached.get(&key).filter(|entry| entry.pid == pid && entry.expires_at > now).copied();
            let future: Pin<Box<dyn Future<Output = (bool, bool)> + Send>> = match current {
                Some(entry) => Box::pin(async move { (entry.is_web, false) }),
                None => {
                    let probe = self.inner.probe.clone();
                    let url = url.to_owned();
                    Box::pin(async move { (probe.is_web(&url).await, true) })
                }
            };
            let shared = future.shared();
            batch.insert(identity, shared.clone());
            shared
        };
        let probed: Vec<(Group, GroupProbes, Option<String>)> = futures::stream::iter(groups)
            .map(|group| {
                let pid = group.server.pid;
                let probes: Vec<(String, ProbeFuture)> = group.urls.iter().map(|url| (url.clone(), probe_of(url, pid))).collect();
                async move {
                    let mut results = Vec::new();
                    let mut visible = None;
                    for (url, probe) in probes {
                        let (is_web, fresh) = probe.await;
                        results.push((web_probe_cache_key(&url), is_web, fresh));
                        if is_web {
                            visible = Some(url);
                            break;
                        }
                    }
                    (group, results, visible)
                }
            })
            .buffered(WEB_PROBE_CONCURRENCY)
            .collect()
            .await;
        let completed_at = (self.inner.clock)();
        let mut next_cache: HashMap<String, ProbeCacheEntry> = cached
            .iter()
            .filter(|(_, entry)| entry.expires_at > completed_at)
            .map(|(key, entry)| (key.clone(), *entry))
            .collect();
        let mut snapshot = ProbeSnapshot {
            discovered: Vec::new(),
            configured: HashMap::new(),
        };
        for (group, results, visible) in probed {
            for (key, is_web, fresh) in results {
                if !fresh {
                    if let Some(entry) = cached.get(&key) {
                        next_cache.insert(key, *entry);
                    }
                } else {
                    next_cache.insert(
                        key,
                        ProbeCacheEntry {
                            pid: group.server.pid,
                            is_web,
                            expires_at: completed_at + WEB_PROBE_CACHE_TTL_MS,
                        },
                    );
                }
            }
            let Some(url) = visible else { continue };
            let server = DiscoveredLocalServer { url, ..group.server };
            match group.configured_key {
                None => snapshot.discovered.push(server),
                Some(key) => {
                    snapshot.configured.insert(key, server);
                }
            }
        }
        *self.inner.cache.lock().unwrap() = next_cache;
        snapshot
    }

    async fn scan_snapshot(&self, configured_urls: &[String]) -> ProbeSnapshot {
        let _scan = self.inner.scan_lock.lock().await;
        let servers = self.listening_servers().await;
        self.probe_servers(servers, configured_urls).await
    }

    /// `scan`: one scan for `configured_urls` (normalized first).
    pub async fn scan(&self, configured_urls: &[String]) -> Vec<DiscoveredLocalServer> {
        let normalized = normalize_configured_urls(configured_urls);
        let snapshot = self.scan_snapshot(&normalized).await;
        project_snapshot(&snapshot, &normalized)
    }

    /// `pollTick`: one scan for every subscriber's URLs; each subscriber whose view changed
    /// hears about it.
    pub async fn poll_tick(&self) {
        let configured: Vec<String> = {
            let state = self.inner.state.lock().unwrap();
            let mut seen = HashSet::new();
            let mut ids: Vec<&u64> = state.subscriptions.keys().collect();
            ids.sort();
            ids.into_iter()
                .flat_map(|id| state.subscriptions[id].configured_urls.iter().cloned())
                .filter(|url| seen.insert(url.clone()))
                .collect()
        };
        let snapshot = self.scan_snapshot(&configured).await;
        let mut state = self.inner.state.lock().unwrap();
        for subscription in state.subscriptions.values_mut() {
            let next = project_snapshot(&snapshot, &subscription.configured_urls);
            if next == subscription.last_snapshot {
                continue;
            }
            subscription.last_snapshot = next.clone();
            let _ = subscription.sender.send(next);
        }
    }

    /// `retain`: polling runs while a guard lives; the first retainer gets an immediate scan.
    pub async fn retain(&self) -> RetainGuard {
        let was_idle = {
            let mut state = self.inner.state.lock().unwrap();
            state.retain_count += 1;
            state.retain_count == 1
        };
        let guard = RetainGuard { inner: self.inner.clone() };
        if was_idle {
            self.poll_tick().await;
        }
        guard
    }

    /// `subscribe`: changes of this subscriber's view, until the guard drops.
    pub fn subscribe(
        &self,
        configured_urls: &[String],
        initial_snapshot: Vec<DiscoveredLocalServer>,
    ) -> (SubscriptionGuard, mpsc::UnboundedReceiver<Vec<DiscoveredLocalServer>>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let id = self.inner.next_subscription.fetch_add(1, Ordering::SeqCst);
        self.inner.state.lock().unwrap().subscriptions.insert(
            id,
            Subscription {
                configured_urls: normalize_configured_urls(configured_urls),
                last_snapshot: initial_snapshot,
                sender,
            },
        );
        (SubscriptionGuard { inner: self.inner.clone(), id }, receiver)
    }

    /// `registerTerminalProcesses`: the process ids running in a terminal (none unregisters).
    pub fn register_terminal_processes(&self, thread_id: &str, terminal_id: &str, process_ids: &[u32]) {
        let owner = TerminalOwner {
            thread_id: thread_id.into(),
            terminal_id: terminal_id.into(),
        };
        let pids: HashSet<u32> = process_ids.iter().copied().filter(|pid| *pid > 0).collect();
        let key = (thread_id.to_owned(), terminal_id.to_owned());
        let mut state = self.inner.state.lock().unwrap();
        if pids.is_empty() {
            state.terminal_processes.remove(&key);
        } else {
            state.terminal_processes.insert(key, (owner, pids));
        }
    }

    /// `unregisterTerminal`.
    pub fn unregister_terminal(&self, thread_id: &str, terminal_id: &str) {
        self.inner
            .state
            .lock()
            .unwrap()
            .terminal_processes
            .remove(&(thread_id.to_owned(), terminal_id.to_owned()));
    }
}

impl Default for PortDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

/// The terminal manager's view of port discovery.
#[async_trait]
impl zc_terminal::TerminalProcessRegistry for PortDiscovery {
    async fn register_terminal_processes(&self, thread_id: &str, terminal_id: &str, process_ids: &[u32]) {
        PortDiscovery::register_terminal_processes(self, thread_id, terminal_id, process_ids);
    }

    async fn unregister_terminal(&self, thread_id: &str, terminal_id: &str) {
        PortDiscovery::unregister_terminal(self, thread_id, terminal_id);
    }
}
