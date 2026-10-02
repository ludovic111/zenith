//! The window's view of the server: the connection, the server's configuration (providers,
//! models, settings), the shell (projects and thread summaries) and the threads open in
//! full. Streams are reopened whenever the connection comes back, resuming after the last
//! sequence seen; the server is woken up (launchd) when it does not answer.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui::{App, AppContext, Context, EventEmitter, SharedString, Task};
use serde_json::{json, Value};
use zc_contracts::{ServerConfig, ServerConfigStreamEvent, ServerProvider, ThreadId};
use zenith_client::{Client, ClientIdentity, ConnectionStatus, RpcError, StreamEvent};
use zenith_model::shell::Shell;
use zenith_model::thread::ThreadState;

pub enum StoreEvent {
    /// A thread open in full changed.
    Thread(ThreadId),
}

#[derive(Clone, Debug)]
pub struct Notice {
    pub id: u64,
    pub message: SharedString,
    pub error: bool,
}

struct OpenThread {
    state: ThreadState,
    task: Option<Task<()>>,
    used: Instant,
}

pub struct Store {
    pub client: Client,
    pub status: ConnectionStatus,
    /// The server answered at least once since the window opened.
    pub ever_connected: bool,
    pub config: Option<ServerConfig>,
    pub shell: Shell,
    threads: HashMap<ThreadId, OpenThread>,
    pub notices: Vec<Notice>,
    next_notice: u64,
    config_task: Option<Task<()>>,
    shell_task: Option<Task<()>>,
    _status_task: Task<()>,
    woke_at: Option<Instant>,
}

impl EventEmitter<StoreEvent> for Store {}

/// Threads kept open in full besides the one on screen.
const OPEN_THREADS: usize = 8;

impl Store {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let client = {
            let _guard = crate::runtime::runtime().enter();
            Client::connect_local(ClientIdentity {
                surface: "desktop",
                app_version: env!("CARGO_PKG_VERSION").into(),
                session_label: "zenith",
            })
        };
        let mut status = client.status();
        let status_task = cx.spawn(async move |this, cx| loop {
            let current = status.borrow_and_update().clone();
            if this.update(cx, |store, cx| store.on_status(current, cx)).is_err() {
                return;
            }
            if status.changed().await.is_err() {
                return;
            }
        });
        Self {
            client,
            status: ConnectionStatus::Connecting,
            ever_connected: false,
            config: None,
            shell: Shell::default(),
            threads: HashMap::new(),
            notices: Vec::new(),
            next_notice: 0,
            config_task: None,
            shell_task: None,
            _status_task: status_task,
            woke_at: None,
        }
    }

    fn on_status(&mut self, status: ConnectionStatus, cx: &mut Context<Self>) {
        let was_connected = self.status == ConnectionStatus::Connected;
        self.status = status.clone();
        match status {
            ConnectionStatus::Connected if !was_connected => {
                self.ever_connected = true;
                self.subscribe_config(cx);
                self.subscribe_shell(cx);
                let ids: Vec<ThreadId> = self.threads.keys().cloned().collect();
                for id in ids {
                    self.subscribe_thread(id, cx);
                }
            }
            ConnectionStatus::Failed(_)
                // Asleep or crashed: launchd brings it back; ask at most once a minute.
                if self.woke_at.is_none_or(|at| at.elapsed() > Duration::from_secs(60)) => {
                    self.woke_at = Some(Instant::now());
                    zenith_client::local::kickstart();
                }
            _ => {}
        }
        cx.notify();
    }

    pub fn connected(&self) -> bool {
        self.status == ConnectionStatus::Connected
    }

    fn subscribe_config(&mut self, cx: &mut Context<Self>) {
        let mut stream = self.client.stream("subscribeServerConfig", json!({}));
        self.config_task = Some(cx.spawn(async move |this, cx| {
            while let Some(event) = stream.next().await {
                let StreamEvent::Item(item) = event else { break };
                let applied = this.update(cx, |store, cx| {
                    store.apply_config(item);
                    cx.notify();
                });
                if applied.is_err() {
                    return;
                }
            }
        }));
    }

    fn apply_config(&mut self, item: Value) {
        match serde_json::from_value::<ServerConfigStreamEvent>(item) {
            Ok(ServerConfigStreamEvent::ServerConfigStreamSnapshotEvent(e)) => self.config = Some(e.config),
            Ok(ServerConfigStreamEvent::ServerConfigStreamProviderStatusesEvent(e)) => {
                if let Some(config) = self.config.as_mut() {
                    config.providers = e.payload.providers;
                }
            }
            Ok(ServerConfigStreamEvent::ServerConfigStreamSettingsUpdatedEvent(e)) => {
                if let Some(config) = self.config.as_mut() {
                    config.settings = e.payload.settings;
                }
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "server config item"),
        }
    }

    fn subscribe_shell(&mut self, cx: &mut Context<Self>) {
        let mut payload = json!({"requestCompletionMarker": true});
        if self.shell.loaded {
            payload["afterSequence"] = json!(self.shell.sequence);
        }
        self.shell.live = false;
        let mut stream = self.client.stream("orchestration.subscribeShell", payload);
        self.shell_task = Some(cx.spawn(async move |this, cx| {
            while let Some(event) = stream.next().await {
                let StreamEvent::Item(item) = event else { break };
                let applied = this.update(cx, |store, cx| {
                    if let Err(error) = store.shell.apply(item) {
                        tracing::warn!(%error, "shell item");
                    }
                    cx.notify();
                });
                if applied.is_err() {
                    return;
                }
            }
        }));
    }

    /// Keeps `id` open in full (subscribing if needed); the oldest unused ones are closed.
    pub fn open_thread(&mut self, id: &ThreadId, cx: &mut Context<Self>) {
        if let Some(open) = self.threads.get_mut(id) {
            open.used = Instant::now();
            return;
        }
        self.threads.insert(
            id.clone(),
            OpenThread {
                state: ThreadState::new(id.clone()),
                task: None,
                used: Instant::now(),
            },
        );
        if self.connected() {
            self.subscribe_thread(id.clone(), cx);
        }
        while self.threads.len() > OPEN_THREADS + 1 {
            let oldest = self.threads.iter().min_by_key(|(_, t)| t.used).map(|(id, _)| id.clone());
            match oldest {
                Some(oldest) if &oldest != id => {
                    self.threads.remove(&oldest);
                }
                _ => break,
            }
        }
    }

    fn subscribe_thread(&mut self, id: ThreadId, cx: &mut Context<Self>) {
        let Some(open) = self.threads.get_mut(&id) else { return };
        let mut payload = json!({"threadId": id.as_str(), "requestCompletionMarker": true, "turnLimit": 30});
        if open.state.loaded {
            payload["afterSequence"] = json!(open.state.sequence());
        }
        open.state.live = false;
        let mut stream = self.client.stream("orchestration.subscribeThread", payload);
        let thread_id = id.clone();
        open.task = Some(cx.spawn(async move |this, cx| {
            while let Some(event) = stream.next().await {
                let StreamEvent::Item(item) = event else { break };
                let applied = this.update(cx, |store, cx| {
                    let Some(open) = store.threads.get_mut(&thread_id) else { return };
                    match open.state.apply(item) {
                        Ok(_) => {
                            cx.emit(StoreEvent::Thread(thread_id.clone()));
                            cx.notify();
                        }
                        Err(error) => tracing::warn!(%error, "thread item"),
                    }
                });
                if applied.is_err() {
                    return;
                }
            }
        }));
    }

    pub fn thread(&self, id: &ThreadId) -> Option<&ThreadState> {
        self.threads.get(id).map(|t| &t.state)
    }

    /// The providers the server reports, enabled first.
    pub fn providers(&self) -> Vec<&ServerProvider> {
        let mut providers: Vec<&ServerProvider> = self.config.iter().flat_map(|c| c.providers.iter()).collect();
        providers.sort_by_key(|p| !p.enabled);
        providers
    }

    pub fn provider(&self, instance_id: &str) -> Option<&ServerProvider> {
        self.config.as_ref()?.providers.iter().find(|p| p.instance_id.as_str() == instance_id)
    }

    pub fn notify_error(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.push_notice(message.into(), true, cx);
    }

    pub fn notify_info(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.push_notice(message.into(), false, cx);
    }

    fn push_notice(&mut self, message: SharedString, error: bool, cx: &mut Context<Self>) {
        self.next_notice += 1;
        let id = self.next_notice;
        self.notices.push(Notice { id, message, error });
        if self.notices.len() > 4 {
            self.notices.remove(0);
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(if error { 8 } else { 4 })).await;
            let _ = this.update(cx, |store, cx| {
                store.notices.retain(|n| n.id != id);
                cx.notify();
            });
        })
        .detach();
    }

    pub fn dismiss_notice(&mut self, id: u64, cx: &mut Context<Self>) {
        self.notices.retain(|n| n.id != id);
        cx.notify();
    }

    /// Runs a registry command (`zenith_commands`), as the CLI and MCP do; a failure shows
    /// as a notice.
    pub fn run_command(&mut self, name: &'static str, params: Value, cx: &mut Context<Self>) -> Task<Result<Value, String>> {
        let client = self.client.clone();
        let task = crate::runtime::spawn(async move {
            zenith_commands::run(&client, zenith_commands::Caller::Window, name, params)
                .await
                .map_err(|e| e.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await.unwrap_or_else(|e| Err(e.to_string()));
            if let Err(error) = &result {
                let message = error.clone();
                let _ = this.update(cx, |store, cx| store.notify_error(message, cx));
            }
            result
        })
    }

    /// A registry command whose failure the caller shows (or not) itself.
    pub fn run_command_quiet(&self, name: &'static str, params: Value) -> impl std::future::Future<Output = Result<Value, String>> + 'static {
        let client = self.client.clone();
        let task = crate::runtime::spawn(async move {
            zenith_commands::run(&client, zenith_commands::Caller::Window, name, params)
                .await
                .map_err(|e| e.to_string())
        });
        async move { task.await.unwrap_or_else(|e| Err(e.to_string())) }
    }

    /// A unary RPC; a failure shows as a notice.
    pub fn call(&mut self, tag: &'static str, payload: Value, cx: &mut Context<Self>) -> Task<Result<Value, RpcError>> {
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let result = client.call(tag, payload).await;
            if let Err(error) = &result {
                let message = format!("{tag}: {error}");
                let _ = this.update(cx, |store, cx| store.notify_error(message, cx));
            }
            result
        })
    }
}

/// `cx.store()` from anywhere: the one store is a global entity.
pub struct GlobalStore(pub gpui::Entity<Store>);

impl gpui::Global for GlobalStore {}

pub fn store(cx: &App) -> gpui::Entity<Store> {
    cx.global::<GlobalStore>().0.clone()
}

pub fn init(cx: &mut App) {
    let store = cx.new(Store::new);
    cx.set_global(GlobalStore(store));
}
