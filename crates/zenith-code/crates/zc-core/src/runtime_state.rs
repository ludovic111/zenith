//! `server-runtime.json` (`apps/server/src/serverRuntimeState.ts`).
//!
//! Written atomically once the server listens, deleted on shutdown. The dashboard (TS and Rust)
//! and the CLI read it to find a live server and to adopt orphans, so the JSON shape and key
//! order are kept: `{"version":1,"pid":…,"host"?,"port":…,"origin":…,"devUrl"?,"startedAt":…,
//! "serviceManaged"?}` followed by a newline.

use std::io::ErrorKind;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// `PersistedServerRuntimeState`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedServerRuntimeState {
    /// Always 1.
    #[serde(deserialize_with = "version_one")]
    pub version: u8,
    pub pid: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub port: i64,
    pub origin: String,
    /// Present when the server fronts a dev web server (`VITE_DEV_SERVER_URL`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dev_url: Option<String>,
    pub started_at: String,
    /// Set when a boot-service launcher supervises the server (never in zenith).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_managed: Option<bool>,
}

fn version_one<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u8, D::Error> {
    let version = u8::deserialize(deserializer)?;
    if version == 1 {
        Ok(version)
    } else {
        Err(serde::de::Error::custom("expected version 1"))
    }
}

/// `ServerRuntimeStateError`.
#[derive(Debug, thiserror::Error)]
#[error("Failed to {operation} server runtime state at {}.", .state_path.display())]
pub struct ServerRuntimeStateError {
    /// `"persist" | "read" | "decode" | "clear"`.
    pub operation: &'static str,
    pub state_path: std::path::PathBuf,
    #[source]
    pub cause: Box<dyn std::error::Error + Send + Sync>,
}

/// `isWildcardHost`.
pub fn is_wildcard_host(host: Option<&str>) -> bool {
    matches!(host, Some("0.0.0.0" | "::" | "[::]"))
}

/// `formatHostForUrl`: bracket bare IPv6 addresses.
pub fn format_host_for_url(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    }
}

/// `runtimeOriginForConfig`: `http://<host>:<port>`, with wildcard or absent hosts reported as
/// `127.0.0.1`.
pub fn runtime_origin(host: Option<&str>, port: u16) -> String {
    let hostname = match host {
        Some(host) if !host.is_empty() && !is_wildcard_host(Some(host)) => format_host_for_url(host),
        _ => "127.0.0.1".to_owned(),
    };
    format!("http://{hostname}:{port}")
}

/// `makePersistedServerRuntimeState`. `dev_url` must already be the serialized URL
/// (`URL.toString()`, see [`crate::config::normalize_url`]).
pub fn make_persisted_server_runtime_state(host: Option<&str>, dev_url: Option<&str>, port: u16, service_managed: bool) -> PersistedServerRuntimeState {
    PersistedServerRuntimeState {
        version: 1,
        pid: std::process::id() as i64,
        host: host.filter(|h| !h.is_empty()).map(str::to_owned),
        port: port as i64,
        origin: runtime_origin(host, port),
        dev_url: dev_url.filter(|u| !u.is_empty()).map(str::to_owned),
        started_at: crate::time::now_iso(),
        service_managed: service_managed.then_some(true),
    }
}

/// The exact file contents: compact JSON plus a newline.
pub fn encode_server_runtime_state(state: &PersistedServerRuntimeState) -> String {
    format!("{}\n", serde_json::to_string(state).expect("runtime state always serializes"))
}

/// `persistServerRuntimeState`: atomic write.
pub async fn persist_server_runtime_state(path: &Path, state: &PersistedServerRuntimeState) -> Result<(), ServerRuntimeStateError> {
    crate::atomic_write::write_file_string_atomically(path, &encode_server_runtime_state(state))
        .await
        .map_err(|cause| ServerRuntimeStateError {
            operation: "persist",
            state_path: path.to_path_buf(),
            cause: Box::new(cause),
        })
}

/// `clearPersistedServerRuntimeState`: remove the file; failures are logged, never returned.
pub async fn clear_persisted_server_runtime_state(path: &Path) {
    match tokio::fs::remove_file(path).await {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            let error = ServerRuntimeStateError {
                operation: "clear",
                state_path: path.to_path_buf(),
                cause: Box::new(error),
            };
            tracing::warn!(operation = error.operation, state_path = %path.display(), "{error}");
        }
    }
}

/// `readPersistedServerRuntimeState`: `None` when missing, blank, unreadable or malformed (the
/// last two are logged as warnings).
pub async fn read_persisted_server_runtime_state(path: &Path) -> Option<PersistedServerRuntimeState> {
    let raw = match tokio::fs::read_to_string(path).await {
        Ok(raw) => raw,
        Err(error) if error.kind() == ErrorKind::NotFound => return None,
        Err(error) => {
            warn_state_error("read", path, Box::new(error));
            return None;
        }
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match serde_json::from_str(trimmed) {
        Ok(state) => Some(state),
        Err(error) => {
            warn_state_error("decode", path, Box::new(error));
            None
        }
    }
}

fn warn_state_error(operation: &'static str, path: &Path, cause: Box<dyn std::error::Error + Send + Sync>) {
    let error = ServerRuntimeStateError {
        operation,
        state_path: path.to_path_buf(),
        cause,
    };
    tracing::warn!(operation, state_path = %path.display(), "{error}");
}

/// `isProcessAlive`: signal 0; `EPERM` (another user's process) still counts as alive.
#[cfg(unix)]
pub fn is_process_alive(pid: i64) -> bool {
    if pid <= 0 || pid > libc::pid_t::MAX as i64 {
        // process.kill(0 | negative) addresses process groups; never treat that as "alive".
        return false;
    }
    // SAFETY: signal 0 performs only the permission/existence check.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
pub fn is_process_alive(_pid: i64) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_rules_match_ts() {
        assert_eq!(runtime_origin(None, 3773), "http://127.0.0.1:3773");
        assert_eq!(runtime_origin(Some("0.0.0.0"), 1), "http://127.0.0.1:1");
        assert_eq!(runtime_origin(Some("::"), 1), "http://127.0.0.1:1");
        assert_eq!(runtime_origin(Some("::1"), 80), "http://[::1]:80");
        assert_eq!(runtime_origin(Some("[::1]"), 80), "http://[::1]:80");
        assert_eq!(runtime_origin(Some("100.64.0.1"), 9), "http://100.64.0.1:9");
    }

    #[test]
    fn encodes_with_ts_key_order_and_optional_keys() {
        let state = PersistedServerRuntimeState {
            version: 1,
            pid: 42,
            host: Some("127.0.0.1".into()),
            port: 3773,
            origin: "http://127.0.0.1:3773".into(),
            dev_url: None,
            started_at: "2026-10-01T12:00:00.000Z".into(),
            service_managed: None,
        };
        assert_eq!(
            encode_server_runtime_state(&state),
            "{\"version\":1,\"pid\":42,\"host\":\"127.0.0.1\",\"port\":3773,\"origin\":\"http://127.0.0.1:3773\",\"startedAt\":\"2026-10-01T12:00:00.000Z\"}\n"
        );
        let made = make_persisted_server_runtime_state(None, Some("http://localhost:5173/"), 9, true);
        let json = encode_server_runtime_state(&made);
        assert!(json.contains("\"devUrl\":\"http://localhost:5173/\""));
        assert!(json.ends_with(",\"serviceManaged\":true}\n"));
        assert!(!json.contains("\"host\""));
    }

    #[tokio::test]
    async fn persist_read_clear_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("userdata/server-runtime.json");
        assert_eq!(read_persisted_server_runtime_state(&path).await, None);
        let state = make_persisted_server_runtime_state(Some("127.0.0.1"), None, 3774, false);
        persist_server_runtime_state(&path, &state).await.unwrap();
        assert_eq!(read_persisted_server_runtime_state(&path).await, Some(state));
        clear_persisted_server_runtime_state(&path).await;
        clear_persisted_server_runtime_state(&path).await;
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn malformed_or_blank_files_read_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server-runtime.json");
        std::fs::write(&path, "  \n").unwrap();
        assert_eq!(read_persisted_server_runtime_state(&path).await, None);
        std::fs::write(&path, "{\"version\":2,\"pid\":1,\"port\":1,\"origin\":\"x\",\"startedAt\":\"y\"}").unwrap();
        assert_eq!(read_persisted_server_runtime_state(&path).await, None);
        // Unknown keys are ignored, like Schema.Struct decoding.
        std::fs::write(
            &path,
            "{\"version\":1,\"pid\":1,\"port\":1,\"origin\":\"x\",\"startedAt\":\"y\",\"extra\":true}",
        )
        .unwrap();
        assert!(read_persisted_server_runtime_state(&path).await.is_some());
    }

    #[test]
    fn this_process_is_alive() {
        assert!(is_process_alive(std::process::id() as i64));
        assert!(!is_process_alive(0));
        assert!(!is_process_alive(-1));
    }
}
