//! Gate 3 of WP-14: `codex app-server` is launched exactly as the TS driver launches it.
//!
//! `tests/fixtures/launch_oracle.json` is produced by the real TS code
//! (`code/apps/server/scripts/codex-launch-oracle.ts`: `makeCodexAdapter(...).startSession` and the
//! status probe's `withCodexAppServerClient`, against a ChildProcessSpawner that records the
//! command). For each case the Rust adapter / probe must compute the same argv, cwd,
//! environment and `extendEnv`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::Value;
use zc_contracts::{CodexSettings, ProviderInstanceId, ProviderSessionStartInput, ThreadId};
use zc_provider_codex::adapter::{CodexAdapter, CodexAdapterOptions, McpProviderSession, McpSessionLookup};
use zc_provider_codex::process::SpawnSpec;
use zc_provider_codex::provider_status::status_probe_input;

struct Lookup(McpProviderSession);

impl McpSessionLookup for Lookup {
    fn read(&self, _thread_id: &ThreadId) -> Option<McpProviderSession> {
        Some(self.0.clone())
    }
}

fn map(value: &Value) -> BTreeMap<String, String> {
    value
        .as_object()
        .map(|object| {
            object
                .iter()
                .map(|(key, value)| (key.clone(), value.as_str().unwrap_or_default().to_owned()))
                .collect()
        })
        .unwrap_or_default()
}

fn rust_spawn(case: &Value) -> SpawnSpec {
    let config: CodexSettings = serde_json::from_value(case["config"].clone()).unwrap();
    let environment = map(&case["environment"]);
    match case["kind"].as_str().unwrap() {
        "session" => {
            let mcp = case.get("mcp").map(|mcp| McpProviderSession {
                endpoint: mcp["endpoint"].as_str().unwrap().to_owned(),
                authorization_header: mcp["authorizationHeader"].as_str().unwrap().to_owned(),
                capabilities: mcp["capabilities"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|cap| cap.as_str().unwrap().to_owned())
                    .collect::<BTreeSet<_>>(),
                agent_device_environment: mcp.get("agentDeviceEnvironment").map(map),
            });
            let adapter = CodexAdapter::new(
                config,
                CodexAdapterOptions {
                    instance_id: case["instanceId"].as_str().map(ProviderInstanceId::new),
                    environment: Some(environment),
                    mcp_sessions: mcp.map(|mcp| Arc::new(Lookup(mcp)) as Arc<dyn McpSessionLookup>),
                    ..CodexAdapterOptions::default()
                },
            );
            let start: ProviderSessionStartInput = serde_json::from_value(case["start"].clone()).unwrap();
            adapter.runtime_options(&start, None).spawn_spec()
        }
        "probe" => status_probe_input(&config, Some(&environment), case["start"]["cwd"].as_str().unwrap()).spawn_spec(),
        other => panic!("unknown case kind {other}"),
    }
}

#[test]
fn launch_argv_and_environment_match_the_typescript_driver() {
    let fixture: Vec<Value> = serde_json::from_str(include_str!("fixtures/launch_oracle.json")).unwrap();
    assert!(fixture.len() >= 8);
    for entry in &fixture {
        let name = entry["case"]["name"].as_str().unwrap();
        let expected = &entry["spawn"];
        let actual = rust_spawn(&entry["case"]);
        assert_eq!(actual.command, expected["command"].as_str().unwrap(), "{name}: command");
        assert_eq!(
            actual.args,
            expected["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|arg| arg.as_str().unwrap().to_owned())
                .collect::<Vec<_>>(),
            "{name}: args"
        );
        assert_eq!(actual.cwd, expected["cwd"].as_str().unwrap(), "{name}: cwd");
        assert_eq!(actual.env, map(&expected["env"]), "{name}: env");
        assert_eq!(actual.extend_env, expected["extendEnv"].as_bool().unwrap(), "{name}: extendEnv");
    }
}
