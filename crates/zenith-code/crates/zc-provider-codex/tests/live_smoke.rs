//! A live smoke test against a real `codex app-server`: the status probe, then ONE trivial turn
//! in a temporary directory with the read-only sandbox. It spends a little of the account's
//! usage, so it never runs by default:
//!
//!   ZC_CODEX_LIVE_BINARY=/path/to/codex ZC_CODEX_LIVE_HOME=/tmp/codex-home-copy \
//!     cargo test -p zc-provider-codex --test live_smoke -- --ignored --nocapture
//!
//! `ZC_CODEX_LIVE_HOME` must be a COPY of a Codex home (at least `auth.json`), never the real
//! `~/.codex`: Codex writes sessions and logs there.

use std::time::Duration;

use serde_json::{json, Value};
use zc_contracts::{CodexSettings, ProviderSendTurnInput, ProviderSessionStartInput, ThreadId};
use zc_ports::adapter::ProviderAdapter;
use zc_provider_codex::adapter::{process_environment, CodexAdapter, CodexAdapterOptions};
use zc_provider_codex::provider_status::check_codex_provider_status;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spends real Codex usage; run explicitly with ZC_CODEX_LIVE_BINARY and ZC_CODEX_LIVE_HOME"]
async fn one_trivial_turn_against_a_real_codex() {
    let (Some(binary), Some(home)) = (std::env::var_os("ZC_CODEX_LIVE_BINARY"), std::env::var_os("ZC_CODEX_LIVE_HOME")) else {
        eprintln!("set ZC_CODEX_LIVE_BINARY and ZC_CODEX_LIVE_HOME (a copy) to run");
        return;
    };
    let home = home.to_string_lossy().into_owned();
    assert!(!home.ends_with("/.codex"), "use a copy of the Codex home, not the real one");
    let settings: CodexSettings = serde_json::from_value(json!({"binaryPath": binary.to_string_lossy(), "homePath": home})).unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let cwd = workdir.path().to_string_lossy().into_owned();

    let draft = check_codex_provider_status(&settings, None, None, None, &cwd, &zc_core::now_iso()).await;
    eprintln!(
        "probe: status={} installed={} version={} auth={} models={} usage={}",
        draft["status"],
        draft["installed"],
        draft["version"],
        draft["auth"],
        draft["models"].as_array().map_or(0, Vec::len),
        draft["usageLimits"]
    );
    assert_eq!(draft["installed"], true, "{draft}");

    let mut environment = process_environment();
    environment.remove("T3CODE_CODEX_LAUNCH_ARGS");
    let adapter = CodexAdapter::new(
        settings,
        CodexAdapterOptions {
            environment: Some(environment),
            ..CodexAdapterOptions::default()
        },
    );
    let mut events = adapter.subscribe();
    let thread = ThreadId::new("live-smoke-thread");
    let start: ProviderSessionStartInput =
        serde_json::from_value(json!({"provider": "codex", "threadId": thread.as_str(), "cwd": cwd, "runtimeMode": "approval-required"})).unwrap();
    let session = adapter.start_session(start).await.unwrap();
    eprintln!("session: {}", serde_json::to_string(&session).unwrap());
    let turn: ProviderSendTurnInput =
        serde_json::from_value(json!({"threadId": thread.as_str(), "input": "Reply with exactly the word pong and nothing else. Do not run any command."}))
            .unwrap();
    let started = adapter.send_turn(turn).await.unwrap();
    eprintln!("turn: {}", serde_json::to_string(&started).unwrap());

    let mut seen: Vec<Value> = Vec::new();
    let completed = tokio::time::timeout(Duration::from_secs(180), async {
        while let Some(event) = events.recv().await {
            let value = serde_json::to_value(&event).unwrap();
            let done = value["type"] == "turn.completed";
            seen.push(value);
            if done {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    for event in &seen {
        let payload = &event["payload"];
        eprintln!(
            "  {} {}",
            event["type"].as_str().unwrap_or_default(),
            [
                &payload["itemType"],
                &payload["state"],
                &payload["delta"],
                &payload["detail"],
                &payload["message"]
            ]
            .iter()
            .filter(|value| !value.is_null())
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join(" ")
        );
    }
    adapter.stop_session(&thread).await.unwrap();
    assert!(completed, "no turn.completed within 180 s");
    let last = seen.last().unwrap();
    assert_eq!(last["payload"]["state"], "completed", "{last}");
    assert!(seen
        .iter()
        .any(|event| event["type"] == "item.completed" && event["payload"]["itemType"] == "assistant_message"));
}
