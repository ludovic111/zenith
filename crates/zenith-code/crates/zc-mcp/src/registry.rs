//! `McpSessionRegistry.ts` + the per-thread store of `McpProviderSession.ts`.
//!
//! Each provider session gets its own bearer credential: 32 random bytes, base64url. Only the
//! SHA-256 (hex) is kept, in memory, so a restart invalidates every credential (the provider
//! sessions are restarted too). A credential lives until [`McpSessionRegistry::revoke_thread`]
//! (or the provider-session / all variants), or until 24 h pass without a sign of life: MCP
//! traffic ([`McpSessionRegistry::resolve`]) and every provider turn
//! ([`McpSessionRegistry::touch`]) count. `/mcp` sits outside the environment auth, so this
//! token is the only thing guarding the toolkits on a remote-reachable server.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::IpAddr;
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use base64::Engine as _;
use sha2::{Digest, Sha256};
use zc_contracts::{ProviderInstanceId, ThreadId};

use crate::provider_session::McpProviderSessionConfig;
use crate::scope::{McpCapability, McpInvocationScope};

/// `DEFAULT_LIVENESS_WINDOW_MS`: 24 h after the last sign of life.
pub const DEFAULT_LIVENESS_WINDOW_MS: i64 = 24 * 60 * 60 * 1_000;

/// `McpCredentialRequest`.
#[derive(Debug, Clone)]
pub struct McpCredentialRequest {
    pub thread_id: String,
    pub provider_instance_id: String,
    /// Capabilities beyond `pull-requests`, which every credential gets.
    pub capabilities: Vec<McpCapability>,
}

/// `McpIssuedCredential`.
#[derive(Debug, Clone)]
pub struct McpIssuedCredential {
    pub config: McpProviderSessionConfig,
}

/// `McpSessionRegistryOptions`.
#[derive(Clone)]
pub struct McpSessionRegistryOptions {
    pub liveness_window_ms: i64,
    /// Milliseconds since the epoch.
    pub now: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl Default for McpSessionRegistryOptions {
    fn default() -> Self {
        Self {
            liveness_window_ms: DEFAULT_LIVENESS_WINDOW_MS,
            now: Arc::new(zc_core::time::now_millis),
        }
    }
}

struct CredentialRecord {
    scope: McpInvocationScope,
    last_alive_at: i64,
}

struct Inner {
    environment_id: String,
    endpoint: RwLock<String>,
    options: McpSessionRegistryOptions,
    /// By token hash.
    records: Mutex<HashMap<String, CredentialRecord>>,
    /// `setMcpProviderSession` / `readMcpProviderSession`, by thread.
    sessions: Mutex<HashMap<String, McpProviderSessionConfig>>,
    /// The environment that puts the `agent-device` CLI on `PATH`, when devices exist.
    agent_device_environment: RwLock<Option<BTreeMap<String, String>>>,
}

/// The registry. Cheap to clone.
#[derive(Clone)]
pub struct McpSessionRegistry {
    inner: Arc<Inner>,
}

fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `getHttpMcpEndpointHost`: a wildcard bind is reachable on loopback, where the provider
/// subprocesses run; anything else is announced as the address it bound.
pub fn endpoint_for(host: Option<&str>, port: u16) -> String {
    let host = host
        .map(|host| host.trim_start_matches('[').trim_end_matches(']'))
        .filter(|host| !host.is_empty());
    let host = match host {
        None => "127.0.0.1".to_owned(),
        Some(host) => match host.parse::<IpAddr>() {
            Ok(ip) if ip.is_unspecified() => "127.0.0.1".to_owned(),
            Ok(IpAddr::V6(ip)) => format!("[{ip}]"),
            Ok(IpAddr::V4(ip)) => ip.to_string(),
            // A host name binds the address it resolves to; loopback for localhost.
            Err(_) if host.eq_ignore_ascii_case("localhost") => "127.0.0.1".to_owned(),
            Err(_) => host.to_owned(),
        },
    };
    format!("http://{host}:{port}/mcp")
}

impl McpSessionRegistry {
    /// A registry for `environment_id`. The endpoint is `http://127.0.0.1/mcp` until
    /// [`Self::set_endpoint`] names the bound address.
    pub fn new(environment_id: impl Into<String>, options: McpSessionRegistryOptions) -> Self {
        Self {
            inner: Arc::new(Inner {
                environment_id: environment_id.into(),
                endpoint: RwLock::new("http://127.0.0.1/mcp".to_owned()),
                options,
                records: Mutex::new(HashMap::new()),
                sessions: Mutex::new(HashMap::new()),
                agent_device_environment: RwLock::new(None),
            }),
        }
    }

    pub fn environment_id(&self) -> &str {
        &self.inner.environment_id
    }

    /// Sets the endpoint credentials announce, from the listener's host and port
    /// ([`endpoint_for`]).
    pub fn set_listen_address(&self, host: Option<&str>, port: u16) {
        self.set_endpoint(endpoint_for(host, port));
    }

    pub fn set_endpoint(&self, endpoint: impl Into<String>) {
        *self.inner.endpoint.write().unwrap() = endpoint.into();
    }

    pub fn endpoint(&self) -> String {
        self.inner.endpoint.read().unwrap().clone()
    }

    /// The `agent-device` environment handed to sessions with the `device` capability (the
    /// device package sets it once its CLI shim exists).
    pub fn set_agent_device_environment(&self, environment: Option<BTreeMap<String, String>>) {
        *self.inner.agent_device_environment.write().unwrap() = environment;
    }

    fn now(&self) -> i64 {
        (self.inner.options.now)()
    }

    fn prune_dead(&self, records: &mut HashMap<String, CredentialRecord>, timestamp: i64) {
        let window = self.inner.options.liveness_window_ms;
        records.retain(|_, record| timestamp - record.last_alive_at <= window);
    }

    /// `issue`: a new credential for the request (pull-requests always granted).
    pub fn issue(&self, request: McpCredentialRequest) -> McpIssuedCredential {
        let issued_at = self.now();
        let provider_session_id = zc_core::ids::uuid_v4();
        let raw_token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(zc_core::ids::random_bytes(32));
        let token_hash = hash_token(&raw_token);
        let mut capabilities: HashSet<McpCapability> = HashSet::from([McpCapability::PullRequests]);
        capabilities.extend(request.capabilities.iter().copied());
        let scope = McpInvocationScope {
            environment_id: self.inner.environment_id.clone(),
            thread_id: request.thread_id.clone(),
            provider_session_id: provider_session_id.clone(),
            provider_instance_id: request.provider_instance_id.clone(),
            capabilities,
            issued_at,
        };
        let capability_names = scope.capability_names().into_iter().map(str::to_owned).collect();
        {
            let mut records = self.inner.records.lock().unwrap();
            self.prune_dead(&mut records, issued_at);
            records.insert(
                token_hash,
                CredentialRecord {
                    scope,
                    last_alive_at: issued_at,
                },
            );
        }
        McpIssuedCredential {
            config: McpProviderSessionConfig {
                environment_id: self.inner.environment_id.clone(),
                thread_id: request.thread_id,
                provider_session_id,
                provider_instance_id: request.provider_instance_id,
                endpoint: self.endpoint(),
                authorization_header: format!("Bearer {raw_token}"),
                capabilities: capability_names,
                agent_device_environment: None,
            },
        }
    }

    /// `resolve`: the scope of a raw bearer token, refreshing its liveness; `None` for an
    /// empty, unknown, revoked or expired token.
    pub fn resolve(&self, raw_token: &str) -> Option<McpInvocationScope> {
        if raw_token.is_empty() {
            return None;
        }
        let token_hash = hash_token(raw_token);
        let timestamp = self.now();
        let mut records = self.inner.records.lock().unwrap();
        self.prune_dead(&mut records, timestamp);
        let record = records.get_mut(&token_hash)?;
        record.last_alive_at = timestamp;
        Some(record.scope.clone())
    }

    /// `touch`: a sign of life for every credential of `thread_id`.
    pub fn touch(&self, thread_id: &str) {
        let timestamp = self.now();
        let mut records = self.inner.records.lock().unwrap();
        self.prune_dead(&mut records, timestamp);
        for record in records.values_mut() {
            if record.scope.thread_id == thread_id {
                record.last_alive_at = timestamp;
            }
        }
    }

    pub fn revoke_provider_session(&self, provider_session_id: &str) {
        self.inner
            .records
            .lock()
            .unwrap()
            .retain(|_, record| record.scope.provider_session_id != provider_session_id);
    }

    pub fn revoke_thread(&self, thread_id: &str) {
        self.inner.records.lock().unwrap().retain(|_, record| record.scope.thread_id != thread_id);
    }

    pub fn revoke_all(&self) {
        self.inner.records.lock().unwrap().clear();
    }

    /// `issueActiveMcpCredential`: revokes the thread's credentials, then issues a new one.
    pub fn issue_for_thread(&self, request: McpCredentialRequest) -> McpIssuedCredential {
        self.revoke_thread(&request.thread_id);
        self.issue(request)
    }

    /// `setMcpProviderSession`.
    pub fn set_provider_session(&self, config: McpProviderSessionConfig) {
        self.inner.sessions.lock().unwrap().insert(config.thread_id.clone(), config);
    }

    /// `readMcpProviderSession`: what the drivers pass to Claude and Codex.
    pub fn read_provider_session(&self, thread_id: &str) -> Option<McpProviderSessionConfig> {
        self.inner.sessions.lock().unwrap().get(thread_id).cloned()
    }

    /// `clearMcpProviderSession`.
    pub fn clear_provider_session(&self, thread_id: &str) {
        self.inner.sessions.lock().unwrap().remove(thread_id);
    }

    /// `clearAllMcpProviderSessions`.
    pub fn clear_all_provider_sessions(&self) {
        self.inner.sessions.lock().unwrap().clear();
    }

    /// `ProviderService.prepareMcpSession`: a fresh credential for the thread, recorded as the
    /// thread's provider session (with the device environment when `device` is granted).
    pub fn prepare_session(&self, thread_id: &str, provider_instance_id: &str, capabilities: &[McpCapability]) -> McpProviderSessionConfig {
        let mut config = self
            .issue_for_thread(McpCredentialRequest {
                thread_id: thread_id.to_owned(),
                provider_instance_id: provider_instance_id.to_owned(),
                capabilities: capabilities.to_vec(),
            })
            .config;
        if capabilities.contains(&McpCapability::Device) {
            config.agent_device_environment = self.inner.agent_device_environment.read().unwrap().clone();
        }
        self.set_provider_session(config.clone());
        config
    }
}

/// The provider service's hook (`issueMcpCredential` + `setMcpProviderSession`, the revoke and
/// touch calls of `ProviderService.ts`).
#[async_trait]
impl zc_providers::hooks::McpSessions for McpSessionRegistry {
    async fn prepare(&self, thread_id: &ThreadId, instance_id: &ProviderInstanceId, capabilities: &[McpCapability]) -> bool {
        self.prepare_session(thread_id.as_str(), instance_id.as_str(), capabilities);
        true
    }

    async fn touch(&self, thread_id: &ThreadId) {
        McpSessionRegistry::touch(self, thread_id.as_str());
    }

    async fn clear(&self, thread_id: &ThreadId) {
        self.revoke_thread(thread_id.as_str());
        self.clear_provider_session(thread_id.as_str());
    }

    async fn revoke_all(&self) {
        McpSessionRegistry::revoke_all(self);
        self.clear_all_provider_sessions();
    }
}

/// What the drivers read when they start a session.
impl zc_providers::hooks::McpSessionReader for McpSessionRegistry {
    fn read(&self, thread_id: &str) -> Option<zc_providers::hooks::McpSessionConfig> {
        self.read_provider_session(thread_id).map(|config| zc_providers::hooks::McpSessionConfig {
            endpoint: config.endpoint,
            authorization_header: config.authorization_header,
            capabilities: config.capabilities,
            agent_device_environment: config.agent_device_environment,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, Ordering};

    use super::*;

    fn registry(clock: Arc<AtomicI64>) -> McpSessionRegistry {
        let registry = McpSessionRegistry::new(
            "environment-1",
            McpSessionRegistryOptions {
                liveness_window_ms: 100,
                now: Arc::new(move || clock.load(Ordering::SeqCst)),
            },
        );
        registry.set_listen_address(Some("127.0.0.1"), 43123);
        registry
    }

    fn request(thread_id: &str, provider: &str, capabilities: &[McpCapability]) -> McpCredentialRequest {
        McpCredentialRequest {
            thread_id: thread_id.into(),
            provider_instance_id: provider.into(),
            capabilities: capabilities.to_vec(),
        }
    }

    fn token(issued: &McpIssuedCredential) -> String {
        issued.config.authorization_header.trim_start_matches("Bearer").trim().to_owned()
    }

    // McpSessionRegistry.test.ts: "stores only a token hash, resolves the bearer token, and
    // revokes by thread"
    #[test]
    fn stores_only_a_token_hash_resolves_the_bearer_token_and_revokes_by_thread() {
        let clock = Arc::new(AtomicI64::new(1_000));
        let registry = registry(clock.clone());
        let issued = registry.issue(request("thread-1", "codex", &[McpCapability::Preview]));
        assert_eq!(issued.config.endpoint, "http://127.0.0.1:43123/mcp");
        let token = token(&issued);
        assert!(token.len() > 20);
        // Only the hash is stored.
        assert!(!registry.inner.records.lock().unwrap().contains_key(&token));
        assert!(registry.inner.records.lock().unwrap().contains_key(&hash_token(&token)));

        assert_eq!(registry.resolve(&token).unwrap().thread_id, "thread-1");
        registry.revoke_thread("thread-1");
        assert_eq!(registry.resolve(&token), None);
    }

    // "always grants pull-requests and gates browser and device access independently"
    #[test]
    fn always_grants_pull_requests_and_gates_browser_and_device_access_independently() {
        let registry = registry(Arc::new(AtomicI64::new(1_000)));
        let capabilities_of = |issued: &McpIssuedCredential| registry.resolve(&token(issued)).unwrap().capability_names();
        let with_preview = registry.issue(request("thread-preview", "codex", &[McpCapability::Preview]));
        let without_preview = registry.issue(request("thread-no-preview", "codex", &[]));
        let with_device = registry.issue(request("thread-device", "codex", &[McpCapability::Device]));
        assert_eq!(capabilities_of(&with_preview), vec!["preview", "pull-requests"]);
        assert_eq!(capabilities_of(&without_preview), vec!["pull-requests"]);
        assert_eq!(capabilities_of(&with_device), vec!["device", "pull-requests"]);
        assert_eq!(with_device.config.capabilities, vec!["device", "pull-requests"]);
    }

    // "builds MCP endpoints from the bound server host"
    #[test]
    fn builds_mcp_endpoints_from_the_bound_server_host() {
        for (host, expected) in [
            ("100.64.0.40", "http://100.64.0.40:43123/mcp"),
            ("0.0.0.0", "http://127.0.0.1:43123/mcp"),
            ("::", "http://127.0.0.1:43123/mcp"),
            ("::1", "http://[::1]:43123/mcp"),
            ("127.0.0.1", "http://127.0.0.1:43123/mcp"),
        ] {
            let registry = registry(Arc::new(AtomicI64::new(1_000)));
            registry.set_listen_address(Some(host), 43123);
            let issued = registry.issue(request(&format!("thread-{host}"), "codex", &[McpCapability::Preview]));
            assert_eq!(issued.config.endpoint, expected, "{host}");
        }
        assert_eq!(endpoint_for(None, 1), "http://127.0.0.1:1/mcp");
        assert_eq!(endpoint_for(Some("[::1]"), 2), "http://[::1]:2/mcp");
        assert_eq!(endpoint_for(Some("localhost"), 3), "http://127.0.0.1:3/mcp");
    }

    // "expires credentials once their session stops showing signs of life"
    #[test]
    fn expires_credentials_once_their_session_stops_showing_signs_of_life() {
        let clock = Arc::new(AtomicI64::new(1_000));
        let registry = registry(clock.clone());
        let issued = registry.issue(request("thread-2", "claude", &[McpCapability::Preview]));
        clock.fetch_add(101, Ordering::SeqCst);
        assert_eq!(registry.resolve(&token(&issued)), None);
    }

    // "keeps a credential alive across turns that never touch an MCP tool"
    #[test]
    fn keeps_a_credential_alive_across_turns_that_never_touch_an_mcp_tool() {
        let clock = Arc::new(AtomicI64::new(1_000));
        let registry = registry(clock.clone());
        let issued = registry.issue(request("thread-3", "claude", &[McpCapability::Preview]));
        for _ in 0..10 {
            clock.fetch_add(99, Ordering::SeqCst);
            registry.touch("thread-3");
        }
        assert_eq!(registry.resolve(&token(&issued)).unwrap().thread_id, "thread-3");
    }

    // "does not keep credentials of other threads alive"
    #[test]
    fn does_not_keep_credentials_of_other_threads_alive() {
        let clock = Arc::new(AtomicI64::new(1_000));
        let registry = registry(clock.clone());
        let issued = registry.issue(request("thread-4", "codex", &[McpCapability::Preview]));
        clock.fetch_add(99, Ordering::SeqCst);
        registry.touch("thread-unrelated");
        clock.fetch_add(2, Ordering::SeqCst);
        assert_eq!(registry.resolve(&token(&issued)), None);
    }

    #[test]
    fn revokes_per_provider_session_and_all() {
        let registry = registry(Arc::new(AtomicI64::new(1_000)));
        let first = registry.issue(request("thread-a", "codex", &[]));
        let second = registry.issue(request("thread-b", "codex", &[]));
        registry.revoke_provider_session(&first.config.provider_session_id);
        assert_eq!(registry.resolve(&token(&first)), None);
        assert!(registry.resolve(&token(&second)).is_some());
        registry.revoke_all();
        assert_eq!(registry.resolve(&token(&second)), None);
        assert_eq!(registry.resolve(""), None);
    }

    #[tokio::test]
    async fn the_provider_hook_issues_one_credential_per_thread_and_clears_it() {
        use zc_providers::hooks::McpSessions;
        let registry = registry(Arc::new(AtomicI64::new(1_000)));
        let thread = ThreadId::new("thread-hook");
        let instance = ProviderInstanceId::new("claudeAgent");
        assert!(
            registry
                .prepare(&thread, &instance, &[McpCapability::PullRequests, McpCapability::Preview])
                .await
        );
        let first = registry.read_provider_session("thread-hook").unwrap();
        assert_eq!(first.capabilities, vec!["preview", "pull-requests"]);
        assert_eq!(first.provider_instance_id, "claudeAgent");
        assert!(registry.prepare(&thread, &instance, &[McpCapability::PullRequests]).await);
        let second = registry.read_provider_session("thread-hook").unwrap();
        // A new session revokes the thread's earlier credential.
        assert_eq!(registry.resolve(first.authorization_header.trim_start_matches("Bearer ")), None);
        assert!(registry.resolve(second.authorization_header.trim_start_matches("Bearer ")).is_some());
        registry.clear(&thread).await;
        assert_eq!(registry.read_provider_session("thread-hook"), None);
        assert_eq!(registry.resolve(second.authorization_header.trim_start_matches("Bearer ")), None);
    }
}
