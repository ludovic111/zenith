//! lsuite discovery (`../lsuite/STANDARD.md` §4): every lsuite app writes
//! `~/.lsuite/apps/<app>.json` when it starts, so the others (and agents) know what is
//! installed and how to drive it. Format 1, as the suite's video app first wrote it (see
//! `../lsuite/STANDARD.md` §4), unknown fields ignored so it can grow:
//!
//! ```json
//! {
//!   "format": 1,
//!   "app": "zenith",
//!   "version": "0.2.0",
//!   "kind": "code",
//!   "appPath": "/Applications/zenith.app",
//!   "executable": "/Applications/zenith.app/Contents/MacOS/zenith",
//!   "cli": "/Applications/zenith.app/Contents/MacOS/zenith-cli",
//!   "mcp": "/Applications/zenith.app/Contents/MacOS/zenith-mcp",
//!   "dataDir": "/Users/me/.zenith",
//!   "running": { "pid": 4242, "port": 4747, "since": "2026-10-02T09:00:00.000Z" },
//!   "bridge": { "url": "http://127.0.0.1:4747", "tokenFile": "/Users/me/.zenith/app/session.token" },
//!   "updatedAt": "2026-10-02T09:00:00.000Z"
//! }
//! ```
//!
//! `mcp` is the MCP server's program, run with `--live` to drive the running app (zenith also
//! reads the older `{ "command", "args" }` form). `running` is `null` once the window closes
//! (zenith's server, its `bridge`, keeps running from login). zenith offers the other apps'
//! MCP servers to the agents of its threads (`zc_core::lsuite` on the server side).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const FORMAT: u32 = 1;

/// An MCP server to start: its program and arguments.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServer {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// `"mcp": "/path/app-mcp"` (format 1) or `{ "command", "args" }`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpField {
    Path(String),
    Server(McpServer),
}

impl McpField {
    pub fn server(&self) -> McpServer {
        match self {
            Self::Path(path) => McpServer {
                command: path.clone(),
                args: vec!["--live".into()],
            },
            Self::Server(server) => server.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Running {
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bridge {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_file: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub format: u32,
    pub app: String,
    pub version: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub app_path: Option<String>,
    #[serde(default)]
    pub executable: Option<String>,
    #[serde(default)]
    pub cli: Option<String>,
    #[serde(default)]
    pub mcp: Option<McpField>,
    #[serde(default)]
    pub data_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub documents: Option<serde_json::Value>,
    #[serde(default)]
    pub running: Option<Running>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge: Option<Bridge>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// `~/.lsuite/apps` (`LSUITE_HOME` replaces `~/.lsuite`).
pub fn apps_dir() -> PathBuf {
    let base = std::env::var_os("LSUITE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".lsuite"));
    base.join("apps")
}

/// The zenith.app bundle this program runs from, if it runs from one.
pub fn bundle_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let bundle = exe.parent()?.parent()?.parent()?;
    (bundle.extension().is_some_and(|e| e == "app")).then(|| bundle.to_path_buf())
}

/// A program next to this one (`zenith-cli`, `zenith-mcp`), if it is there.
pub fn sibling(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let path = exe.parent()?.join(name);
    path.is_file().then_some(path)
}

/// What zenith writes about itself; `running` while its window is open.
pub fn zenith_info(version: &str, running: bool) -> AppInfo {
    let as_string = |p: PathBuf| p.to_string_lossy().into_owned();
    let base_url = zenith_client::local::base_url();
    let port = base_url.rsplit(':').next().and_then(|p| p.trim_end_matches('/').parse().ok());
    let now = zenith_client::now_iso();
    AppInfo {
        format: FORMAT,
        app: "zenith".into(),
        version: version.into(),
        kind: Some("code".into()),
        app_path: bundle_path().map(as_string),
        executable: std::env::current_exe().ok().map(as_string),
        cli: sibling("zenith-cli").map(as_string),
        mcp: sibling("zenith-mcp").map(|p| McpField::Path(as_string(p))),
        data_dir: zenith_client::local::app_home().parent().map(|p| as_string(p.to_path_buf())),
        documents: None,
        running: running.then(|| Running {
            pid: std::process::id(),
            port,
            control_file: None,
            since: Some(now.clone()),
        }),
        bridge: Some(Bridge {
            url: base_url,
            token_file: Some(as_string(zenith_client::local::token_path())),
        }),
        updated_at: Some(now),
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(tmp, path)
}

/// Writes `~/.lsuite/apps/zenith.json` (`running`: the window is open).
pub fn write_discovery(version: &str, running: bool) -> anyhow::Result<PathBuf> {
    let path = apps_dir().join("zenith.json");
    let json = serde_json::to_vec_pretty(&zenith_info(version, running))?;
    write_atomic(&path, &json)?;
    Ok(path)
}

/// Every other lsuite app that wrote its file (malformed or future-format files are skipped).
pub fn installed_apps() -> Vec<AppInfo> {
    let Ok(entries) = std::fs::read_dir(apps_dir()) else {
        return Vec::new();
    };
    let mut apps: Vec<AppInfo> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read(e.path()).ok())
        .filter_map(|bytes| serde_json::from_slice::<AppInfo>(&bytes).ok())
        .filter(|a| a.format == FORMAT && a.app != "zenith")
        .collect();
    apps.sort_by(|a, b| a.app.cmp(&b.app));
    apps
}

/// The MCP servers of the other installed apps whose command exists, for the agents.
pub fn mcp_servers() -> Vec<(String, McpServer)> {
    installed_apps()
        .into_iter()
        .filter_map(|app| {
            let mcp = app.mcp?.server();
            Path::new(&mcp.command).is_file().then_some((app.app, mcp))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_round_trips_and_skips_strangers() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LSUITE_HOME", dir.path());
        let written = write_discovery("9.9.9", true).unwrap();
        let mine: AppInfo = serde_json::from_slice(&std::fs::read(&written).unwrap()).unwrap();
        assert_eq!(mine.app, "zenith");
        assert_eq!(mine.format, 1);
        assert_eq!(mine.kind.as_deref(), Some("code"));
        assert!(mine.running.is_some());
        // Another app's format 1 file (mcp as a plain path, extra fields).
        std::fs::write(
            apps_dir().join("made-up-app.json"),
            br#"{"format":1,"app":"made-up-app","version":"1.0.0","kind":"video","mcp":"/nonexistent/made-up-mcp","dataDir":"/tmp/x","documents":{"extensions":["json"]},"running":null,"updatedAt":"2026-10-02T07:52:15.735036Z"}"#,
        )
        .unwrap();
        std::fs::write(apps_dir().join("broken.json"), b"{nope").unwrap();
        let apps = installed_apps();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].app, "made-up-app");
        assert_eq!(apps[0].mcp.as_ref().unwrap().server().args, vec!["--live"]);
        // Its MCP binary does not exist: not offered.
        assert!(mcp_servers().is_empty());
        std::env::remove_var("LSUITE_HOME");
    }
}
