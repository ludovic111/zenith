//! Connects to the local server (ZENITH_URL, ZENITH_CODE_HOME, ZENITH_APP_HOME apply), prints
//! the providers and the shell, and creates a project when given `--add-project <path>`.

use std::time::Duration;

use serde_json::json;
use zenith_client::{Client, ClientIdentity, StreamEvent};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = Client::connect_local(ClientIdentity {
        surface: "cli",
        app_version: env!("CARGO_PKG_VERSION").into(),
        session_label: "zenith probe",
    });
    client.wait_connected(Duration::from_secs(15)).await?;
    println!("connected to {}", client.base_url());

    let mut config = client.stream("subscribeServerConfig", json!({}));
    if let Some(StreamEvent::Item(item)) = config.next().await {
        let providers = item["config"]["providers"].as_array().cloned().unwrap_or_default();
        for p in providers {
            println!(
                "provider {} ({}) status={} models={}",
                p["instanceId"],
                p["driver"],
                p["status"],
                p["models"].as_array().map(|m| m.len()).unwrap_or(0)
            );
        }
    }

    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--add-project") {
        let path = &args[i + 1];
        let seq = client
            .dispatch(json!({
                "type": "project.create",
                "projectId": zenith_client::new_id(),
                "title": std::path::Path::new(path).file_name().unwrap().to_string_lossy(),
                "workspaceRoot": path,
            }))
            .await?;
        println!("project.create → sequence {seq}");
    }

    let mut shell = client.stream("orchestration.subscribeShell", json!({"requestCompletionMarker": true}));
    while let Some(event) = shell.next().await {
        match event {
            StreamEvent::Item(item) => {
                let kind = item["kind"].as_str().unwrap_or("?");
                if kind == "snapshot" {
                    let s = &item["snapshot"];
                    println!(
                        "shell snapshot: {} projects, {} threads (sequence {})",
                        s["projects"].as_array().map_or(0, |a| a.len()),
                        s["threads"].as_array().map_or(0, |a| a.len()),
                        s["snapshotSequence"]
                    );
                } else {
                    println!("shell item: {kind}");
                }
                if kind == "synchronized" {
                    break;
                }
            }
            StreamEvent::End(result) => {
                println!("shell ended: {result:?}");
                break;
            }
        }
    }
    Ok(())
}
