//! zenith-specific: the other lsuite apps' MCP servers, offered to the agents of every thread.
//!
//! lsuite apps describe themselves in `~/.lsuite/apps/<app>.json` (format 1, see
//! `crates/zenith-commands/src/lsuite.rs` and `../lsuite/STANDARD.md` §4). Each app whose file
//! names an `mcp` command that exists is added to the Claude (`--mcp-config`) and Codex
//! (`-c mcp_servers.<app>…`) sessions as a stdio server, so an agent in zenith can drive
//! the suite's other apps. zenith itself is left out (its threads already have their own MCP), and
//! `ZENITH_NO_LSUITE_MCP=1` turns it off.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LsuiteMcpServer {
    /// The app's name (`made-up-app`), used as the MCP server's name.
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
}

#[derive(Deserialize)]
struct AppFile {
    format: u32,
    app: String,
    #[serde(default)]
    mcp: Option<McpFile>,
}

/// `"mcp": "/path/app-mcp"` (format 1, run with `--live`) or `{ "command", "args" }`.
#[derive(Deserialize)]
#[serde(untagged)]
enum McpFile {
    Path(String),
    Server {
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },
}

fn apps_dir() -> PathBuf {
    std::env::var_os("LSUITE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".lsuite"))
        .join("apps")
}

/// A name usable as an MCP server key and a TOML bare key.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The MCP servers to offer, sorted by app name.
pub fn mcp_servers() -> Vec<LsuiteMcpServer> {
    if std::env::var("ZENITH_NO_LSUITE_MCP").is_ok_and(|v| !v.is_empty() && v != "0") {
        return Vec::new();
    }
    mcp_servers_in(&apps_dir())
}

pub fn mcp_servers_in(dir: &Path) -> Vec<LsuiteMcpServer> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut servers: Vec<LsuiteMcpServer> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read(e.path()).ok())
        .filter_map(|bytes| serde_json::from_slice::<AppFile>(&bytes).ok())
        .filter(|app| app.format == 1 && app.app != "zenith" && is_plain_name(&app.app))
        .filter_map(|app| {
            let (command, args) = match app.mcp? {
                McpFile::Path(command) => (command, vec!["--live".to_owned()]),
                McpFile::Server { command, args } => (command, args),
            };
            Path::new(&command).is_file().then_some(LsuiteMcpServer { name: app.app, command, args })
        })
        .collect();
    servers.sort_by(|a, b| a.name.cmp(&b.name));
    servers.dedup_by(|a, b| a.name == b.name);
    servers
}

/// A TOML basic string (Codex's `-c key=value` values).
pub fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_installed_apps_with_an_mcp_command() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("made-up-mcp");
        std::fs::write(&binary, b"#!/bin/sh\n").unwrap();
        let write = |name: &str, json: String| std::fs::write(dir.path().join(format!("{name}.json")), json).unwrap();
        write(
            "made-up",
            format!(
                r#"{{"format":1,"app":"made-up","version":"1.0.0","mcp":{{"command":"{}","args":["--live"]}}}}"#,
                binary.display()
            ),
        );
        write(
            "plain",
            format!(
                r#"{{"format":1,"app":"plain","version":"1","kind":"video","mcp":"{}","running":null}}"#,
                binary.display()
            ),
        );
        write(
            "missing",
            r#"{"format":1,"app":"missing","version":"1","mcp":{"command":"/nonexistent/x"}}"#.into(),
        );
        write(
            "zenith",
            format!(r#"{{"format":1,"app":"zenith","version":"1","mcp":{{"command":"{}"}}}}"#, binary.display()),
        );
        write(
            "future",
            format!(r#"{{"format":2,"app":"future","version":"1","mcp":{{"command":"{}"}}}}"#, binary.display()),
        );
        write(
            "odd name",
            format!(r#"{{"format":1,"app":"odd name","version":"1","mcp":{{"command":"{}"}}}}"#, binary.display()),
        );
        let servers = mcp_servers_in(dir.path());
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "made-up");
        assert_eq!(servers[0].args, vec!["--live"]);
        assert_eq!(servers[1].name, "plain");
        assert_eq!(servers[1].args, vec!["--live"]);
        assert_eq!(toml_string("a \"b\" \\c"), r#""a \"b\" \\c""#);
    }
}
