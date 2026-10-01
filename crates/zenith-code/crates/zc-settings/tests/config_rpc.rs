//! The RPCs end to end over the zc-rpc engine: `server.getSettings`, `server.updateSettings`,
//! `server.upsertKeybinding`, `server.removeKeybinding`, `server.getConfig` and
//! `subscribeServerConfig` (with a contributor and an event source plugged in, standing in for
//! the environment/auth/providers packages).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc;
use futures::stream::BoxStream;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Map, Value};
use zc_rpc::{AuthContext, ConnectionSetup, Inbound, Outbound, RpcRouter, RpcServer};
use zc_settings::config::{
    observability, ConfigError, ConfigEventSource, ConfigOptions, ServerConfigParts, ServerConfigService, SnapshotContributor, SERVER_CONFIG_KEYS,
};
use zc_settings::keybindings::KeybindingsService;
use zc_settings::rpc::SettingsRpc;
use zc_settings::settings::{ServerSettingsService, SECRET_REDACTED};
use zc_settings::themes::EnvironmentThemeService;

/// Fills `environment`, `auth` and `providers` like the packages that own them will.
struct FakeEnvironment;

#[async_trait]
impl SnapshotContributor for FakeEnvironment {
    async fn contribute(&self, config: &mut Map<String, Value>, _options: &ConfigOptions) -> Result<(), ConfigError> {
        config.insert("environment".into(), json!({"environmentId": "environment-test"}));
        config.insert("auth".into(), json!({"policy": "loopback-browser"}));
        config.insert("providers".into(), json!([{"instanceId": "codex"}]));
        config.insert("availableEditors".into(), json!(["vscode"]));
        Ok(())
    }
}

/// `providerStatuses` from a channel the test drives.
#[derive(Default)]
struct FakeProviderStatuses {
    senders: Mutex<Vec<futures::channel::mpsc::UnboundedSender<Value>>>,
}

impl FakeProviderStatuses {
    fn publish(&self, providers: Value) {
        for sender in self.senders.lock().unwrap().iter() {
            let _ = sender.unbounded_send(json!({"version": 1, "type": "providerStatuses", "payload": {"providers": providers}}));
        }
    }
}

impl ConfigEventSource for FakeProviderStatuses {
    fn subscribe(&self, _options: &ConfigOptions) -> Option<BoxStream<'static, Value>> {
        let (sender, receiver) = futures::channel::mpsc::unbounded();
        self.senders.lock().unwrap().push(sender);
        Some(receiver.boxed())
    }
}

struct Harness {
    dir: tempfile::TempDir,
    server: Arc<RpcServer>,
    statuses: Arc<FakeProviderStatuses>,
    secrets: zc_core::ServerSecretStore,
}

impl Harness {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let paths = zc_core::derive_server_paths(dir.path(), None, true);
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        let secrets = zc_core::ServerSecretStore::open(&paths.secrets_dir).await.unwrap();
        let settings = ServerSettingsService::new(&paths.settings_path, Arc::new(secrets.clone()), Arc::new(zc_db::Db::open_in_memory().unwrap()));
        settings.start().await.unwrap();
        let keybindings = KeybindingsService::new(&paths.keybindings_config_path);
        keybindings.start().await.unwrap();
        let themes = EnvironmentThemeService::start(&paths.environment_themes_dir).await;
        let statuses = Arc::new(FakeProviderStatuses::default());
        let config = ServerConfigService::new(
            settings,
            keybindings,
            ServerConfigParts {
                cwd: "/tmp/project".into(),
                observability: observability(&paths.logs_dir.to_string_lossy(), None, None, None),
            },
        )
        .with_themes(themes)
        .with_contributor(Arc::new(FakeEnvironment))
        .with_event_source(statuses.clone());
        let router: RpcRouter = SettingsRpc::new(config).register(RpcRouter::builder()).build().unwrap();
        Self {
            dir,
            server: RpcServer::new(router),
            statuses,
            secrets,
        }
    }

    fn themes_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("userdata").join("themes")
    }
}

struct Client {
    tx: mpsc::UnboundedSender<Inbound>,
    rx: mpsc::UnboundedReceiver<Outbound>,
    /// Stream items received but not consumed yet (chunks batch several items).
    pending: std::collections::VecDeque<Value>,
}

impl Client {
    fn connect(server: &Arc<RpcServer>) -> Self {
        let (tx, in_rx) = mpsc::unbounded();
        let (out_tx, rx) = mpsc::unbounded();
        let server = server.clone();
        let setup = ConnectionSetup {
            auth: AuthContext::new(["orchestration:read", "orchestration:operate"]),
            ..Default::default()
        };
        tokio::spawn(async move {
            server.serve_socket(setup, in_rx, out_tx.sink_map_err(|_| ())).await;
        });
        Self {
            tx,
            rx,
            pending: Default::default(),
        }
    }

    fn request(&self, id: u64, tag: &str, payload: Value) {
        let frame = json!({"_tag": "Request", "id": id, "tag": tag, "payload": payload, "headers": []});
        self.tx.unbounded_send(Inbound::Text(frame.to_string())).unwrap();
    }

    async fn recv(&mut self) -> Value {
        match tokio::time::timeout(Duration::from_secs(10), self.rx.next()).await {
            Ok(Some(Outbound::Text(text))) => serde_json::from_str(&text).unwrap(),
            other => panic!("expected a frame, got {other:?}"),
        }
    }

    /// One unary call: the `Exit`.
    async fn call(&mut self, id: u64, tag: &str, payload: Value) -> Value {
        self.request(id, tag, payload);
        let frame = self.recv().await;
        assert_eq!(frame["_tag"], json!("Exit"), "{frame}");
        frame["exit"].clone()
    }

    /// The next stream items (one chunk), acknowledged.
    async fn chunk(&mut self, id: u64) -> Vec<Value> {
        loop {
            let frame = self.recv().await;
            if frame["_tag"] == json!("Chunk") && frame["requestId"] == json!(id) {
                let ack = json!({"_tag": "Ack", "requestId": id});
                self.tx.unbounded_send(Inbound::Text(ack.to_string())).unwrap();
                return frame["values"].as_array().unwrap().clone();
            }
            assert_ne!(frame["_tag"], json!("Exit"), "subscribeServerConfig must never complete: {frame}");
        }
    }

    /// The next stream item.
    async fn item(&mut self, id: u64) -> Value {
        loop {
            if let Some(item) = self.pending.pop_front() {
                return item;
            }
            let items = self.chunk(id).await;
            self.pending.extend(items);
        }
    }

    /// Stream items until one of `kind` arrives.
    async fn until(&mut self, id: u64, kind: &str) -> Value {
        loop {
            let item = self.item(id).await;
            if item["type"] == json!(kind) {
                return item;
            }
        }
    }
}

#[tokio::test]
async fn get_config_assembles_the_snapshot_in_contract_order() {
    let harness = Harness::new().await;
    let mut client = Client::connect(&harness.server);
    let exit = client.call(1, "server.getConfig", json!({})).await;
    assert_eq!(exit["_tag"], json!("Success"));
    let config = &exit["value"];
    let keys: Vec<&str> = config.as_object().unwrap().keys().map(String::as_str).collect();
    let expected: Vec<&str> = SERVER_CONFIG_KEYS.iter().copied().filter(|key| config.get(*key).is_some()).collect();
    assert_eq!(keys, expected);
    assert_eq!(config["environment"]["environmentId"], json!("environment-test"));
    assert_eq!(config["cwd"], json!("/tmp/project"));
    assert!(config["keybindingsConfigPath"].as_str().unwrap().ends_with("keybindings.json"));
    assert_eq!(config["keybindings"].as_array().unwrap().len(), 72);
    assert_eq!(config["issues"], json!([]));
    assert_eq!(config["providers"], json!([{"instanceId": "codex"}]));
    assert_eq!(config["observability"]["localTracingEnabled"], json!(true));
    assert_eq!(config["observability"]["otlpTracesEnabled"], json!(false));
    assert_eq!(config["settings"]["responseStreamingMode"], json!("paragraph"));
    assert_eq!(config["shellResumeCompletionMarker"], json!(true));
    assert!(config.get("environmentThemes").is_none());
}

#[tokio::test]
async fn settings_rpcs_redact_secrets_and_report_tagged_errors() {
    let harness = Harness::new().await;
    let mut client = Client::connect(&harness.server);
    let exit = client
        .call(
            1,
            "server.updateSettings",
            json!({"patch": {"bitbucket": {"email": "me@example.com", "apiToken": "bb-api"}, "usageLimitSources": {"hub": {"kind": "cliproxy", "url": "http://hub.example", "managementKey": "hub-key"}}}}),
        )
        .await;
    assert_eq!(exit["_tag"], json!("Success"));
    assert_eq!(
        exit["value"]["bitbucket"],
        json!({"email": "me@example.com", "accessToken": "", "apiToken": SECRET_REDACTED})
    );
    assert_eq!(exit["value"]["usageLimitSources"]["hub"]["managementKey"], json!(SECRET_REDACTED));
    assert_eq!(harness.secrets.get("bitbucket-api-token").await.unwrap().as_deref(), Some(&b"bb-api"[..]));

    let exit = client.call(2, "server.getSettings", json!({})).await;
    assert_eq!(exit["value"]["bitbucket"]["apiToken"], json!(SECRET_REDACTED));

    // A payload that does not decode dies per request.
    let exit = client
        .call(3, "server.updateSettings", json!({"patch": {"enableAgentBrowserAccess": "yes"}}))
        .await;
    assert_eq!(exit["cause"][0]["_tag"], json!("Die"), "{exit}");

    // So does one that fails a schema refinement the generated types do not check.
    let exit = client
        .call(
            4,
            "server.updateSettings",
            json!({"patch": {"usageLimitSources": {"hub": {"kind": "cliproxy", "url": "  "}}}}),
        )
        .await;
    assert_eq!(exit["cause"][0]["_tag"], json!("Die"), "{exit}");

    // A settings failure is a typed failure: a secret that cannot be read (a directory stands
    // where the file should be), before anything is written.
    let secrets_dir = harness.dir.path().join("userdata").join("secrets");
    let api_token = secrets_dir.join("bitbucket-api-token.bin");
    std::fs::remove_file(&api_token).unwrap();
    std::fs::create_dir_all(api_token.join("blocker")).unwrap();
    let exit = client
        .call(5, "server.updateSettings", json!({"patch": {"bitbucket": {"apiToken": "new"}}}))
        .await;
    assert_eq!(exit["cause"][0]["_tag"], json!("Fail"), "{exit}");
    assert_eq!(exit["cause"][0]["error"]["_tag"], json!("ServerSettingsError"));
    assert_eq!(exit["cause"][0]["error"]["operation"], json!("read-secret"));
}

#[tokio::test]
async fn keybinding_rpcs_upsert_and_remove() {
    let harness = Harness::new().await;
    let mut client = Client::connect(&harness.server);
    let exit = client
        .call(
            1,
            "server.upsertKeybinding",
            json!({"key": "mod+shift+y", "command": "chat.new", "when": "!terminalFocus"}),
        )
        .await;
    assert_eq!(exit["_tag"], json!("Success"));
    assert_eq!(exit["value"]["issues"], json!([]));
    let last = exit["value"]["keybindings"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["command"], json!("chat.new"));
    assert_eq!(last["shortcut"]["key"], json!("y"));
    let exit = client
        .call(
            2,
            "server.removeKeybinding",
            json!({"key": "mod+shift+y", "command": "chat.new", "when": "!terminalFocus"}),
        )
        .await;
    assert_eq!(exit["_tag"], json!("Success"));
    assert!(!exit["value"]["keybindings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|rule| rule["shortcut"]["key"] == json!("y")));
}

#[tokio::test]
async fn subscribe_server_config_streams_the_snapshot_then_every_change() {
    let harness = Harness::new().await;
    let mut client = Client::connect(&harness.server);
    let mut writer = Client::connect(&harness.server);
    client.request(1, "subscribeServerConfig", json!({"environmentThemes": true}));
    let first = client.item(1).await;
    assert_eq!(first["version"], json!(1));
    assert_eq!(first["type"], json!("snapshot"));
    assert_eq!(first["config"]["environment"]["environmentId"], json!("environment-test"));
    // The theme stream emits the current set right away (never in the snapshot).
    let themes = client.until(1, "environmentThemesUpdated").await;
    assert_eq!(themes["payload"], json!({"themes": []}));

    writer
        .call(10, "server.updateSettings", json!({"patch": {"addProjectBaseDirectory": "~/code"}}))
        .await;
    let settings = client.until(1, "settingsUpdated").await;
    assert_eq!(settings["payload"]["settings"]["addProjectBaseDirectory"], json!("~/code"));

    writer
        .call(11, "server.upsertKeybinding", json!({"key": "mod+shift+y", "command": "chat.new"}))
        .await;
    let keybindings = client.until(1, "keybindingsUpdated").await;
    assert_eq!(keybindings["payload"]["issues"], json!([]));
    assert!(keybindings["payload"]["keybindings"].as_array().unwrap().len() >= 72);

    harness.statuses.publish(json!([{"instanceId": "codex", "status": "ready"}]));
    let statuses = client.until(1, "providerStatuses").await;
    assert_eq!(statuses["payload"]["providers"][0]["status"], json!("ready"));

    tokio::time::sleep(Duration::from_millis(200)).await;
    std::fs::write(
        harness.themes_dir().join("nightfall.json"),
        r##"{"name":"Nightfall","appearance":"dark","canvas":"#1a1b26","accent":"#7aa2f7"}"##,
    )
    .unwrap();
    let themes = client.until(1, "environmentThemesUpdated").await;
    assert_eq!(themes["payload"]["themes"][0]["id"], json!("nightfall"));
}

#[tokio::test]
async fn subscribe_server_config_sends_themes_only_to_subscribers_that_ask() {
    let harness = Harness::new().await;
    let mut client = Client::connect(&harness.server);
    let mut writer = Client::connect(&harness.server);
    client.request(1, "subscribeServerConfig", json!({}));
    assert_eq!(client.item(1).await["type"], json!("snapshot"));
    writer
        .call(10, "server.updateSettings", json!({"patch": {"addProjectBaseDirectory": "~/code"}}))
        .await;
    // Everything up to the settings change: no theme event in between.
    loop {
        let item = client.item(1).await;
        assert_ne!(item["type"], json!("environmentThemesUpdated"));
        if item["type"] == json!("settingsUpdated") {
            break;
        }
    }
}
