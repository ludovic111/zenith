//! zenith-mcp: zenith's commands as an MCP server (stdio, JSON-RPC 2.0, one message per
//! line), for any agent: `claude mcp add zenith -- <path>/zenith-mcp --live`.
//!
//! `--live` drives the running zenith (the local server, woken up if needed); zenith keeps
//! its state in the server, not in a document file, so there is no `--file` mode. Tool names
//! are the command names with `_` for `.` (`thread_new`). What agents may run is set in
//! zenith › Settings › Agents (off, read, full) and checked for every call.

use std::io::{BufRead, Write};
use std::time::Duration;

use serde_json::{json, Value};
use zenith_client::{Client, ClientIdentity};
use zenith_commands::permissions::{Access, Permissions};
use zenith_commands::registry::{self, Caller, Effect, COMMANDS};

const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

fn tool_name(command: &str) -> String {
    command.replace('.', "_")
}

fn command_name(tool: &str) -> Option<&'static str> {
    COMMANDS.iter().find(|s| tool_name(s.name) == tool || s.name == tool).map(|s| s.name)
}

fn tools() -> Value {
    let access = Permissions::load().mcp;
    let tools: Vec<Value> = COMMANDS
        .iter()
        .filter(|spec| match access {
            Access::Off => false,
            Access::Read => spec.effect == Effect::Read,
            Access::Full => spec.name != "settings.setAgentPermissions",
        })
        .map(|spec| {
            json!({
                "name": tool_name(spec.name),
                "description": spec.summary,
                "inputSchema": registry::input_schema(spec),
                "annotations": {
                    "readOnlyHint": spec.effect == Effect::Read,
                    "destructiveHint": spec.effect == Effect::Destructive,
                },
            })
        })
        .collect();
    json!({"tools": tools})
}

async fn handle(client: &Client, message: &Value) -> Option<Value> {
    let id = message.get("id").cloned();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    // Notifications get no answer.
    let id = id?;
    let result: Result<Value, (i64, String)> = match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSIONS[0]);
            let version = if PROTOCOL_VERSIONS.contains(&asked) { asked } else { PROTOCOL_VERSIONS[0] };
            Ok(json!({
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "zenith", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "zenith runs coding agents (Claude Code, Codex) in threads, per project. Start with project_overview, then thread_new / thread_send (with wait: true to get the outcome), thread_get, thread_approve and thread_answer.",
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools()),
        "tools/call" => {
            let tool = params.get("name").and_then(Value::as_str).unwrap_or("");
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            match command_name(tool) {
                None => Err((-32602, format!("unknown tool {tool:?}"))),
                Some(name) => Ok(match registry::run(client, Caller::Agent, name, arguments).await {
                    Ok(value) => json!({
                        "content": [{"type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default()}],
                        "structuredContent": if value.is_object() { value.clone() } else { json!({"result": value}) },
                        "isError": false,
                    }),
                    Err(error) => json!({"content": [{"type": "text", "text": error.to_string()}], "isError": true}),
                }),
            }
        }
        "resources/list" => Ok(json!({"resources": []})),
        "prompts/list" => Ok(json!({"prompts": []})),
        other => Err((-32601, format!("method not found: {other}"))),
    };
    Some(match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("zenith-mcp {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "zenith-mcp --live: zenith's commands as MCP tools over stdio.\nAdd it to Claude Code: claude mcp add zenith -- {} --live",
            std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "zenith-mcp".into())
        );
        return;
    }
    if let Some(file) = args.iter().position(|a| a == "--file") {
        eprintln!("zenith-mcp: zenith has no document file ({:?}); use --live.", args.get(file + 1));
        std::process::exit(2);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let client = Client::connect_local(ClientIdentity {
            surface: "cli",
            app_version: env!("CARGO_PKG_VERSION").into(),
            session_label: "zenith",
        });
        if client.wait_connected(Duration::from_secs(4)).await.is_err() {
            zenith_client::local::kickstart();
            let _ = client.wait_connected(Duration::from_secs(20)).await;
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        while let Some(line) = rx.recv().await {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let reply = match serde_json::from_str::<Value>(line) {
                Ok(Value::Array(batch)) => {
                    let mut replies = Vec::new();
                    for message in &batch {
                        if let Some(reply) = handle(&client, message).await {
                            replies.push(reply);
                        }
                    }
                    (!replies.is_empty()).then(|| Value::Array(replies))
                }
                Ok(message) => handle(&client, &message).await,
                Err(error) => Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {error}")}})),
            };
            if let Some(reply) = reply {
                let mut out = std::io::stdout().lock();
                let _ = writeln!(out, "{reply}");
                let _ = out.flush();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_round_trip() {
        for spec in COMMANDS {
            assert_eq!(command_name(&tool_name(spec.name)), Some(spec.name));
            assert!(tool_name(spec.name).chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        }
    }
}
