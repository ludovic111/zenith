//! A thread: its timeline (your messages, the agent's answers in Markdown, what it did
//! grouped into sentences that open into the details, its plans, the files each turn
//! changed and their diffs), what it waits on (approvals, questions), and the composer.
//!
//! Its header shows the git state where it works (changes, ahead/behind, its pull request)
//! with the commit, push and pull request actions, the project's scripts, and its terminals
//! (a panel at the bottom, ⌘J).
//!
//! The same view starts a thread: a draft with a project and a thread id made here; the
//! first message creates the thread on the server (`thread.turn.start` with
//! `bootstrap.createThread`, and `prepareWorktree` for a new worktree).

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use gpui::prelude::*;
use gpui::{
    canvas, div, list, px, AnyElement, App, ClipboardItem, Context, Corner, Entity, EventEmitter, Focusable, FontWeight, Hsla, ListAlignment, ListState,
    Pixels, Point, SharedString, Subscription, WeakEntity, Window,
};
use serde_json::{json, Value};
use zc_contracts::{OrchestrationMessageRole, OrchestrationThread, ProjectId, ThreadId};
use zenith_model::requests::{pending_requests, PendingApproval, PendingUserInput};
use zenith_model::shell::{self, ThreadStatus};
use zenith_model::time::{duration, millis, now_millis};
use zenith_model::timeline::{self, TimelineItem, WorkGroup};
use zenith_model::worklog::{self, Action, WorkEntry};

use crate::assets::{Icon, MONO_FONT};
use crate::composer::{Composer, ComposerEvent, EnvMode, ModelChoice};
use crate::store::{self, Store, StoreEvent};
use crate::terminal::{TermStatus, TerminalPanel};
use crate::theme::{radius, text, ActiveTheme};
use crate::ui::menu::{Entry, OpenMenu};
use crate::ui::{caps_label, icon, markdown, pill, spinner, Button, Variant};

/// The timeline's reading width.
const COLUMN: f32 = 780.;

pub enum ThreadViewEvent {
    /// The draft's first message created this thread.
    Created(ThreadId),
}

enum Target {
    Draft { project: ProjectId, thread: ThreadId },
    Thread(ThreadId),
}

enum DiffState {
    Loading,
    Loaded(String),
    Failed(String),
}

#[derive(Default, Clone)]
struct Answer {
    selected: Vec<String>,
}

pub struct ThreadView {
    store: Entity<Store>,
    target: Target,
    composer: Entity<Composer>,
    list: ListState,
    items: Vec<TimelineItem>,
    fingerprints: Vec<u64>,
    expanded: HashSet<String>,
    diffs: HashMap<String, DiffState>,
    answers: HashMap<String, Answer>,
    /// The message sent, shown until the server's copy arrives.
    pending_send: Option<String>,
    sending: bool,
    composer_synced: bool,
    this: WeakEntity<Self>,
    /// `git.status` where the thread works (`vcs.refreshStatus`), once known.
    git: Option<Value>,
    /// The git action under way ("Committing…").
    git_busy: Option<SharedString>,
    menu: Option<OpenMenu>,
    terminals: Vec<Entity<TerminalPanel>>,
    active_terminal: usize,
    terminal_visible: bool,
    was_running: bool,
    /// The bottom column's width, from the last frame.
    width: Option<Pixels>,
    _subscriptions: Vec<Subscription>,
    /// Redraws "Working for…" every second while the agent works.
    _tick: gpui::Task<()>,
}

impl EventEmitter<ThreadViewEvent> for ThreadView {}

impl ThreadView {
    pub fn existing(id: ThreadId, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = store::store(cx);
        store.update(cx, |s, cx| s.open_thread(&id, cx));
        let mut this = Self::new(Target::Thread(id), "Ask for a change, or reply…", window, cx);
        this.rebuild(cx);
        this.refresh_git(cx);
        this
    }

    pub fn draft(project: ProjectId, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let thread = ThreadId::from(zenith_client::new_id().as_str());
        let mut this = Self::new(Target::Draft { project, thread }, "What should the agent do?", window, cx);
        this.refresh_git(cx);
        this.composer.update(cx, |c, cx| {
            c.env_mode = Some(EnvMode::Local);
            c.model = Composer::default_model(cx);
            if let Some(config) = store::store(cx).read(cx).config.as_ref() {
                c.runtime_mode = config.settings.default_runtime_mode.as_str().to_owned();
            }
        });
        this
    }

    fn new(target: Target, placeholder: &str, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = store::store(cx);
        let composer = cx.new(|cx| Composer::new(placeholder, window, cx));
        let subscriptions = vec![
            cx.subscribe_in(&composer, window, Self::on_composer_event),
            cx.subscribe(&store, |this, _, event: &StoreEvent, cx| {
                let StoreEvent::Thread(id) = event;
                if this.thread_id() == id {
                    this.rebuild(cx);
                    // A turn ended: what it changed shows in the git bar.
                    let running = this.store.read(cx).thread(id).is_some_and(|t| t.is_running());
                    if this.was_running && !running {
                        this.refresh_git(cx);
                    }
                    this.was_running = running;
                }
            }),
            cx.observe(&store, |this, _, cx| {
                this.sync_composer(cx);
                cx.notify();
            }),
        ];
        Self {
            store,
            target,
            composer,
            list: ListState::new(0, ListAlignment::Bottom, px(800.)),
            items: Vec::new(),
            fingerprints: Vec::new(),
            expanded: HashSet::new(),
            diffs: HashMap::new(),
            answers: HashMap::new(),
            pending_send: None,
            sending: false,
            composer_synced: false,
            this: cx.weak_entity(),
            git: None,
            git_busy: None,
            menu: None,
            terminals: Vec::new(),
            active_terminal: 0,
            terminal_visible: false,
            was_running: false,
            width: None,
            _subscriptions: subscriptions,
            _tick: cx.spawn(async move |this, cx| loop {
                cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
                let alive = this.update(cx, |this, cx| {
                    if this.store.read(cx).thread(this.thread_id()).is_some_and(|t| t.is_running()) {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
            }),
        }
    }

    pub fn thread_id(&self) -> &ThreadId {
        match &self.target {
            Target::Draft { thread, .. } => thread,
            Target::Thread(id) => id,
        }
    }

    pub fn project_id(&self, cx: &App) -> Option<ProjectId> {
        match &self.target {
            Target::Draft { project, .. } => Some(project.clone()),
            Target::Thread(id) => self.store.read(cx).shell.thread(id).map(|t| t.project_id.clone()),
        }
    }

    fn is_draft(&self) -> bool {
        matches!(self.target, Target::Draft { .. })
    }

    fn thread<'a>(&self, cx: &'a App) -> Option<&'a OrchestrationThread> {
        self.store.read(cx).thread(self.thread_id())?.thread()
    }

    pub fn focus_composer(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.composer.read(cx).focus(window, cx);
    }

    /// The composer starts from the thread's model and modes, once they are known.
    fn sync_composer(&mut self, cx: &mut Context<Self>) {
        if self.composer_synced || self.is_draft() {
            let running = self.store.read(cx).thread(self.thread_id()).is_some_and(|t| t.is_running());
            self.composer.update(cx, |c, cx| {
                if c.running != running {
                    c.running = running;
                    cx.notify();
                }
            });
            return;
        }
        let Some(thread) = self.thread(cx).cloned() else { return };
        self.composer_synced = true;
        let model = serde_json::to_value(&thread.model_selection).ok().and_then(|v| ModelChoice::from_json(&v));
        let running = self.store.read(cx).thread(self.thread_id()).is_some_and(|t| t.is_running());
        self.composer.update(cx, |c, cx| {
            c.model = model;
            c.runtime_mode = thread.runtime_mode.as_str().to_owned();
            c.plan_mode = thread.interaction_mode.as_str() == "plan";
            c.running = running;
            cx.notify();
        });
    }

    fn fingerprint(item: &TimelineItem) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        item.key().hash(&mut h);
        match item {
            TimelineItem::Message(m) => {
                m.text.len().hash(&mut h);
                m.streaming.hash(&mut h);
                m.updated_at.hash(&mut h);
            }
            TimelineItem::Plan(p) => {
                p.updated_at.hash(&mut h);
                p.implemented_at.hash(&mut h);
            }
            TimelineItem::Work(g) => {
                g.entries.len().hash(&mut h);
                g.running.hash(&mut h);
                g.failed.hash(&mut h);
                g.summary.hash(&mut h);
            }
            TimelineItem::Diff(d) => {
                d.completed_at.hash(&mut h);
                d.files.len().hash(&mut h);
            }
        }
        h.finish()
    }

    /// Rebuilds the timeline, remeasuring only what changed.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let Some(thread) = self.thread(cx) else {
            cx.notify();
            return;
        };
        let items = timeline::timeline(thread);
        if let Some(pending) = &self.pending_send {
            if thread
                .messages
                .iter()
                .any(|m| m.role == OrchestrationMessageRole::User && m.text.trim() == pending.trim())
            {
                self.pending_send = None;
            }
        }
        let fingerprints: Vec<u64> = items.iter().map(Self::fingerprint).collect();
        let first_change = self
            .fingerprints
            .iter()
            .zip(&fingerprints)
            .position(|(a, b)| a != b)
            .unwrap_or(self.fingerprints.len().min(fingerprints.len()));
        if first_change < self.fingerprints.len() || fingerprints.len() != self.fingerprints.len() {
            self.list.splice(first_change..self.fingerprints.len(), fingerprints.len() - first_change);
        }
        self.items = items;
        self.fingerprints = fingerprints;
        self.sync_composer(cx);
        cx.notify();
    }

    fn remeasure(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.items.len() {
            self.list.splice(index..index + 1, 1);
        }
        cx.notify();
    }

    fn toggle(&mut self, key: String, index: usize, cx: &mut Context<Self>) {
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
        self.remeasure(index, cx);
    }

    /// Runs a registry command (`zenith_commands`), as the CLI and agents would.
    fn command(&mut self, name: &'static str, params: Value, cx: &mut Context<Self>) -> gpui::Task<Result<Value, String>> {
        self.store.update(cx, |s, cx| s.run_command(name, params, cx))
    }

    fn on_composer_event(&mut self, _: &Entity<Composer>, event: &ComposerEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            ComposerEvent::Send(text) => self.send(text.clone(), None, window, cx),
            ComposerEvent::Stop => self.stop(cx),
        }
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        let params = json!({"threadId": self.thread_id().as_str()});
        self.command("thread.interrupt", params, cx).detach();
    }

    /// Sends a message (`thread.send`); the first one of a draft creates the thread
    /// (`thread.new`, with the id this view already has). `implement_plan` is a plan's id.
    fn send(&mut self, text: String, implement_plan: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if self.sending {
            return;
        }
        let composer = self.composer.read(cx);
        let Some(model) = composer.model.clone() else {
            self.store
                .update(cx, |s, cx| s.notify_error("Choose a model first (no provider is ready?)", cx));
            return;
        };
        let mut params = json!({
            "prompt": text,
            "threadId": self.thread_id().as_str(),
            "provider": model.instance_id,
            "model": model.model,
            "runtimeMode": composer.runtime_mode,
            "plan": implement_plan.is_none() && composer.plan_mode,
        });
        if !composer.images.is_empty() {
            params["images"] = Value::Array(composer.images.iter().map(|p| json!(p.to_string_lossy())).collect());
        }
        if !model.options.is_empty() {
            params["modelOptions"] = Value::Array(model.options.iter().map(|(id, value)| json!({"id": id, "value": value})).collect());
        }
        let name = match &self.target {
            Target::Draft { project, .. } => {
                params["projectId"] = json!(project.as_str());
                params["worktree"] = json!(composer.env_mode == Some(EnvMode::Worktree));
                "thread.new"
            }
            Target::Thread(_) => {
                if let Some(plan) = &implement_plan {
                    params["implementPlan"] = json!(plan);
                }
                "thread.send"
            }
        };
        self.sending = true;
        self.pending_send = Some(text);
        self.composer.update(cx, |c, cx| c.clear(cx));
        let task = self.command(name, params, cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.sending = false;
                match result {
                    Ok(_) => {
                        if let Target::Draft { thread, .. } = &this.target {
                            let id = thread.clone();
                            this.target = Target::Thread(id.clone());
                            this.composer.update(cx, |c, _| c.env_mode = None);
                            this.store.update(cx, |s, cx| s.open_thread(&id, cx));
                            this.composer_synced = true;
                            cx.emit(ThreadViewEvent::Created(id));
                        }
                    }
                    Err(_) => {
                        // Give the text back to try again.
                        if let Some(text) = this.pending_send.take() {
                            this.composer.update(cx, |c, cx| c.editor.update(cx, |e, cx| e.set_text(text, cx)));
                        }
                    }
                }
                this.focus_composer(window, cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn respond_approval(&mut self, approval: &PendingApproval, decision: &str, cx: &mut Context<Self>) {
        let params = json!({"threadId": self.thread_id().as_str(), "requestId": approval.request_id, "decision": decision});
        self.command("thread.approve", params, cx).detach();
    }

    fn submit_answers(&mut self, input: &PendingUserInput, cx: &mut Context<Self>) {
        let mut answers = serde_json::Map::new();
        for question in &input.questions {
            let key = format!("{}:{}", input.request_id, question.id);
            let answer = self.answers.get(&key).cloned().unwrap_or_default();
            if answer.selected.is_empty() {
                return;
            }
            let value = if question.multi_select {
                json!(answer.selected)
            } else {
                json!(answer.selected[0])
            };
            answers.insert(question.id.clone(), value);
        }
        let params = json!({"threadId": self.thread_id().as_str(), "requestId": input.request_id, "answers": Value::Object(answers)});
        self.command("thread.answer", params, cx).detach();
    }

    fn load_diff(&mut self, key: String, turn_count: i64, index: usize, cx: &mut Context<Self>) {
        if self.diffs.contains_key(&key) {
            self.toggle(key, index, cx);
            return;
        }
        self.diffs.insert(key.clone(), DiffState::Loading);
        self.expanded.insert(key.clone());
        self.remeasure(index, cx);
        let params = json!({"threadId": self.thread_id().as_str(), "fromTurn": (turn_count - 1).max(0), "toTurn": turn_count});
        let task = self.command("thread.diff", params, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let state = match result {
                    Ok(value) => DiffState::Loaded(value.get("diff").and_then(Value::as_str).unwrap_or("").to_owned()),
                    Err(error) => DiffState::Failed(error),
                };
                this.diffs.insert(key, state);
                let index = index.min(this.items.len().saturating_sub(1));
                this.remeasure(index, cx);
            });
        })
        .detach();
    }

    /// Where git commands act: the thread (its worktree, else its project's folder), or the
    /// draft's project.
    fn git_target(&self) -> Value {
        match &self.target {
            Target::Draft { project, .. } => json!({"projectId": project.as_str()}),
            Target::Thread(id) => json!({"threadId": id.as_str()}),
        }
    }

    pub fn refresh_git(&mut self, cx: &mut Context<Self>) {
        let status = self.store.read(cx).run_command_quiet("git.status", self.git_target());
        cx.spawn(async move |this, cx| {
            let result = status.await;
            this.update(cx, |this, cx| {
                if let Ok(value) = result {
                    this.git = value.get("status").cloned();
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// `commit`, `push`, `pr`, `commit_push`, `commit_push_pr` (`git.commit`, messages
    /// written by the server) or `pull` (`git.pull`).
    pub fn git_action(&mut self, action: &'static str, cx: &mut Context<Self>) {
        if self.git_busy.is_some() {
            return;
        }
        let mut params = self.git_target();
        let (name, busy) = match action {
            "pull" => ("git.pull", "Pulling…"),
            "push" => ("git.commit", "Pushing…"),
            "pr" | "commit_push_pr" => ("git.commit", "Opening the pull request…"),
            "commit_push" => ("git.commit", "Committing and pushing…"),
            _ => ("git.commit", "Committing…"),
        };
        if name == "git.commit" {
            params["action"] = json!(action);
        }
        self.git_busy = Some(busy.into());
        cx.notify();
        let task = self.command(name, params, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.git_busy = None;
                if let Ok(value) = result {
                    let toast = value.pointer("/result/toast/title").and_then(Value::as_str).map(str::to_owned);
                    let message = match (name, toast) {
                        (_, Some(toast)) => toast,
                        ("git.pull", None) => "Pulled".to_owned(),
                        _ => "Done".to_owned(),
                    };
                    this.store.update(cx, |s, cx| s.notify_info(message, cx));
                    if let Some(url) = value.pointer("/result/pr/url").and_then(Value::as_str) {
                        cx.open_url(url);
                    }
                }
                this.refresh_git(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn open_git_menu(&mut self, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let git = self.git.clone().unwrap_or(Value::Null);
        let changes = git.get("hasWorkingTreeChanges").and_then(Value::as_bool).unwrap_or(false);
        let ahead = git.get("aheadCount").and_then(Value::as_i64).unwrap_or(0);
        let behind = git.get("behindCount").and_then(Value::as_i64).unwrap_or(0);
        let upstream = git.get("hasUpstream").and_then(Value::as_bool).unwrap_or(false);
        let remote = git.get("hasPrimaryRemote").and_then(Value::as_bool).unwrap_or(upstream);
        let pr = git.get("pr").filter(|p| !p.is_null()).cloned();
        let this = self.this.clone();
        let item = move |label: &str, icon: Icon, action: &'static str, enabled: bool| {
            let this = this.clone();
            Entry::item(label.to_owned(), move |_, cx| {
                this.update(cx, |v, cx| v.git_action(action, cx)).ok();
            })
            .icon(icon)
            .disabled(!enabled)
        };
        let mut entries = vec![
            item("Commit", Icon::GitCommit, "commit", changes),
            item("Commit and push", Icon::Push, "commit_push", changes && remote),
            item(
                "Commit, push and open a pull request",
                Icon::PullRequest,
                "commit_push_pr",
                changes && remote && pr.is_none(),
            ),
            Entry::Separator,
            item(
                &format!("Push{}", if ahead > 0 { format!(" ({ahead})") } else { String::new() }),
                Icon::Push,
                "push",
                remote && (ahead > 0 || !upstream),
            ),
            item(
                &format!("Pull{}", if behind > 0 { format!(" ({behind})") } else { String::new() }),
                Icon::Pull,
                "pull",
                upstream,
            ),
        ];
        if pr.is_none() {
            entries.push(item("Open a pull request", Icon::PullRequest, "pr", remote && !changes));
        }
        if let Some(url) = pr.as_ref().and_then(|p| p.get("url")).and_then(Value::as_str).map(str::to_owned) {
            let number = pr.as_ref().and_then(|p| p.get("number")).and_then(Value::as_i64).unwrap_or_default();
            entries.push(Entry::item(format!("Open pull request #{number}"), move |_, cx| cx.open_url(&url)).icon(Icon::ExternalLink));
        }
        entries.push(Entry::Separator);
        let this = self.this.clone();
        entries.push(
            Entry::item("Refresh", move |_, cx| {
                this.update(cx, |v, cx| v.refresh_git(cx)).ok();
            })
            .icon(Icon::Refresh),
        );
        self.open_menu(entries, position, window, cx);
    }

    fn open_scripts_menu(&mut self, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project_id(cx) else { return };
        let scripts = self.store.read(cx).shell.project(&project).map(|p| p.scripts.clone()).unwrap_or_default();
        let entries = scripts
            .into_iter()
            .map(|script| {
                let this = self.this.clone();
                let id = script.id.to_string();
                Entry::item(script.name.to_string(), move |window, cx| {
                    this.update(cx, |v, cx| v.run_script(id.clone(), window, cx)).ok();
                })
                .icon(Icon::Play)
                .detail(script.command.to_string())
            })
            .collect();
        self.open_menu(entries, position, window, cx);
    }

    fn open_menu(&mut self, entries: Vec<Entry>, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let mut menu = OpenMenu::new(entries, position, window, cx, |this, _, _| this.menu = None);
        menu.corner = Corner::TopRight;
        self.menu = Some(menu);
        cx.notify();
    }

    /// Runs a project script (`project.runScript`) and shows its terminal.
    pub fn run_script(&mut self, script_id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_draft() {
            return;
        }
        let params = json!({"threadId": self.thread_id().as_str(), "scriptId": script_id});
        let task = self.command("project.runScript", params, cx);
        cx.spawn_in(window, async move |this, cx| {
            let Ok(result) = task.await else { return };
            let terminal = result.get("terminalId").and_then(Value::as_str).unwrap_or("default").to_owned();
            let title = result.get("script").and_then(Value::as_str).unwrap_or("Script").to_owned();
            this.update_in(cx, |this, window, cx| this.show_terminal(terminal, title, None, window, cx))
                .ok();
        })
        .detach();
    }

    /// Shows the thread's terminal `id` (opened on the server if needed), typing `command`.
    pub fn show_terminal(&mut self, id: String, title: String, command: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_draft() {
            return;
        }
        match self.terminals.iter().position(|t| t.read(cx).terminal_id == id) {
            Some(index) => {
                self.active_terminal = index;
                let panel = self.terminals[index].clone();
                panel.update(cx, |panel, cx| {
                    if !panel.running() {
                        panel.restart(cx);
                    }
                    if let Some(command) = command {
                        panel.write(format!("{command}\r"));
                    }
                });
            }
            None => {
                let thread = self.thread_id().clone();
                let panel = cx.new(|cx| TerminalPanel::new(thread, id, title, command, cx));
                self.terminals.push(panel);
                self.active_terminal = self.terminals.len() - 1;
            }
        }
        self.terminal_visible = true;
        if let Some(panel) = self.terminals.get(self.active_terminal) {
            window.focus(&panel.focus_handle(cx));
        }
        cx.notify();
    }

    /// ⌘J: shows the terminal (the thread's default one at first), or hides it.
    pub fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_draft() {
            return;
        }
        if self.terminal_visible {
            self.terminal_visible = false;
            self.focus_composer(window, cx);
            cx.notify();
        } else if self.terminals.is_empty() {
            self.show_terminal("default".into(), "Terminal".into(), None, window, cx);
        } else {
            let id = self.terminals[self.active_terminal.min(self.terminals.len() - 1)].read(cx).terminal_id.clone();
            self.show_terminal(id, String::new(), None, window, cx);
        }
    }

    fn close_terminal(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.terminals.len() {
            return;
        }
        let panel = self.terminals.remove(index);
        panel.update(cx, |panel, cx| panel.close(cx));
        if self.terminals.is_empty() {
            self.terminal_visible = false;
            self.focus_composer(window, cx);
        }
        self.active_terminal = self.active_terminal.min(self.terminals.len().saturating_sub(1));
        cx.notify();
    }

    /// The git state and the thread's tools, at the right of the title bar.
    fn render_tools(&self, cx: &App) -> AnyElement {
        let c = cx.theme().colors.clone();
        let git = self.git.as_ref().filter(|g| g.get("isRepo").and_then(Value::as_bool).unwrap_or(false));
        let num = |key: &str| git.and_then(|g| g.get(key)).and_then(Value::as_i64).unwrap_or(0);
        let files = git
            .and_then(|g| g.pointer("/workingTree/files"))
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        let (insertions, deletions) = (
            git.and_then(|g| g.pointer("/workingTree/insertions")).and_then(Value::as_i64).unwrap_or(0),
            git.and_then(|g| g.pointer("/workingTree/deletions")).and_then(Value::as_i64).unwrap_or(0),
        );
        let (ahead, behind) = (num("aheadCount"), num("behindCount"));
        let pr = git.and_then(|g| g.get("pr")).filter(|p| !p.is_null()).cloned();
        let has_scripts = self
            .project_id(cx)
            .and_then(|p| self.store.read(cx).shell.project(&p).map(|p| !p.scripts.is_empty()))
            .unwrap_or(false);
        let draft = self.is_draft();
        let this = self.this.clone();
        let this_scripts = self.this.clone();
        let this_terminal = self.this.clone();
        let small = |label: String, color: Hsla| div().flex_none().text_size(px(text::SM)).text_color(color).child(SharedString::from(label));

        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.))
            .when(files > 0, |this| {
                this.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(5.))
                        .font_family(MONO_FONT)
                        .child(small(format!("{files} file{}", if files == 1 { "" } else { "s" }), c.text_3))
                        .when(insertions > 0, |this| this.child(small(format!("+{insertions}"), c.success)))
                        .when(deletions > 0, |this| this.child(small(format!("−{deletions}"), c.danger))),
                )
            })
            .when(ahead > 0, |this| this.child(small(format!("↑{ahead}"), c.text_2)))
            .when(behind > 0, |this| this.child(small(format!("↓{behind}"), c.warning)))
            .when_some(pr, |this, pr| {
                let number = pr.get("number").and_then(Value::as_i64).unwrap_or_default();
                let state = pr.get("state").and_then(Value::as_str).unwrap_or("open").to_owned();
                let url = pr.get("url").and_then(Value::as_str).unwrap_or_default().to_owned();
                let color = match state.as_str() {
                    "merged" => c.accent_text,
                    "closed" => c.danger,
                    _ => c.success,
                };
                this.child(div().id("thread-pr").cursor_pointer().on_click(move |_, _, cx| cx.open_url(&url)).child(pill(
                    format!("#{number} {state}"),
                    color,
                    Hsla { a: 0.14, ..color },
                )))
            })
            .when_some(self.git_busy.clone(), |this, busy| {
                this.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .text_size(px(text::SM))
                        .text_color(c.text_2)
                        .child(spinner("git-busy", c.accent, 12.))
                        .child(busy),
                )
            })
            .when(git.is_some(), |el| {
                el.child(
                    Button::new("thread-git")
                        .icon(Icon::GitCommit)
                        .small()
                        .tooltip("Commit, push, pull request")
                        .on_click(move |event, window, cx| {
                            this.update(cx, |v, cx| v.open_git_menu(event.position(), window, cx)).ok();
                        }),
                )
            })
            .when(has_scripts && !draft, |el| {
                el.child(
                    Button::new("thread-scripts")
                        .icon(Icon::Play)
                        .small()
                        .tooltip("Run a project script")
                        .on_click(move |event, window, cx| {
                            this_scripts.update(cx, |v, cx| v.open_scripts_menu(event.position(), window, cx)).ok();
                        }),
                )
            })
            .when(!draft, |el| {
                el.child(
                    Button::new("thread-terminal")
                        .icon(Icon::SquareTerminal)
                        .small()
                        .selected(self.terminal_visible)
                        .tooltip_keys("Terminal", "⌘J")
                        .on_click(move |_, window, cx| {
                            this_terminal.update(cx, |v, cx| v.toggle_terminal(window, cx)).ok();
                        }),
                )
            })
            .into_any_element()
    }

    /// The terminals at the bottom: their tabs, then the active one.
    fn render_terminals(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.terminal_visible || self.terminals.is_empty() {
            return None;
        }
        let c = cx.theme().colors.clone();
        let active = self.active_terminal.min(self.terminals.len() - 1);
        let panel = self.terminals[active].clone();
        let tabs: Vec<AnyElement> = self
            .terminals
            .iter()
            .enumerate()
            .map(|(index, terminal)| {
                let t = terminal.read(cx);
                let title = if t.title.is_empty() {
                    SharedString::from("Terminal")
                } else {
                    t.title.clone()
                };
                let dot = match &t.status {
                    TermStatus::Running | TermStatus::Starting => c.success,
                    TermStatus::Exited(Some(0)) | TermStatus::Exited(None) => c.text_3,
                    _ => c.danger,
                };
                let selected = index == active;
                div()
                    .id(("terminal-tab", index))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(8.))
                    .h(px(24.))
                    .rounded(px(radius::SM))
                    .cursor_pointer()
                    .text_size(px(text::SM))
                    .text_color(if selected { c.text } else { c.text_3 })
                    .when(selected, |this| this.bg(c.hover))
                    .hover(|this| this.bg(c.hover))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.active_terminal = index;
                        if let Some(panel) = this.terminals.get(index) {
                            window.focus(&panel.focus_handle(cx));
                        }
                        cx.notify();
                    }))
                    .child(div().size(px(6.)).rounded_full().bg(dot))
                    .child(title)
                    .child(
                        div()
                            .id(("terminal-close", index))
                            .rounded(px(radius::XS))
                            .hover(|this| this.bg(c.line))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_terminal(index, window, cx);
                            }))
                            .child(icon(Icon::X, c.text_3).size(px(11.))),
                    )
                    .into_any_element()
            })
            .collect();
        let status = match &panel.read(cx).status {
            TermStatus::Exited(code) => Some(match code {
                Some(code) => format!("exited ({code})"),
                None => "exited".to_owned(),
            }),
            TermStatus::Failed(error) => Some(error.clone()),
            _ => None,
        };
        let this = self.this.clone();
        let restart = panel.clone();
        Some(
            div()
                .flex_none()
                .h(px(280.))
                .flex()
                .flex_col()
                .border_t_1()
                .border_color(c.line)
                .bg(c.bg_sunken)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .px(px(8.))
                        .py(px(4.))
                        .children(tabs)
                        .child(
                            Button::new("terminal-new")
                                .icon(Icon::Plus)
                                .small()
                                .tooltip("New terminal")
                                .on_click(move |_, window, cx| {
                                    this.update(cx, |v, cx| {
                                        let n = v.terminals.len() + 1;
                                        v.show_terminal(format!("term-{}", zenith_client::new_id()), format!("Terminal {n}"), None, window, cx);
                                    })
                                    .ok();
                                }),
                        )
                        .child(div().flex_1())
                        .when_some(status, |this, status| {
                            this.child(div().text_size(px(text::SM)).text_color(c.text_3).child(SharedString::from(status)))
                                .child(
                                    Button::new("terminal-restart")
                                        .icon(Icon::Revert)
                                        .small()
                                        .tooltip("Restart")
                                        .on_click(move |_, _, cx| restart.update(cx, |p, cx| p.restart(cx))),
                                )
                        })
                        .child({
                            let copy = panel.clone();
                            Button::new("terminal-copy")
                                .icon(Icon::Copy)
                                .small()
                                .tooltip("Copy what it shows")
                                .on_click(move |_, _, cx| {
                                    let text = copy.read(cx).contents();
                                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                                })
                        })
                        .child(
                            Button::new("terminal-hide")
                                .icon(Icon::ChevronDown)
                                .small()
                                .tooltip_keys("Hide", "⌘J")
                                .on_click(cx.listener(|this, _, window, cx| this.toggle_terminal(window, cx))),
                        ),
                )
                .child(div().flex_1().min_h_0().child(panel))
                .into_any_element(),
        )
    }

    /// The title bar's content for this thread.
    pub fn render_header(&self, cx: &App) -> AnyElement {
        let c = cx.theme().colors.clone();
        let store = self.store.read(cx);
        let (title, project, branch, status): (SharedString, Option<String>, Option<String>, Option<ThreadStatus>) = match &self.target {
            Target::Draft { project, .. } => ("New thread".into(), store.shell.project(project).map(|p| p.title.clone()), None, None),
            Target::Thread(id) => match store.shell.thread(id) {
                Some(t) => (
                    t.title.clone().into(),
                    store.shell.project(&t.project_id).map(|p| p.title.clone()),
                    t.branch.clone(),
                    Some(shell::status(t)),
                ),
                None => ("Loading…".into(), None, None, None),
            },
        };
        div()
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap(px(10.))
            .child(
                div()
                    .truncate()
                    .text_size(px(text::MD))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(c.text)
                    .child(title),
            )
            .when_some(project, |this, project| {
                this.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .flex_none()
                        .text_size(px(text::SM))
                        .text_color(c.text_3)
                        .child(icon(Icon::Folder, c.text_3).size(px(12.)))
                        .child(project),
                )
            })
            .when_some(branch, |this, branch| {
                this.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .min_w_0()
                        .text_size(px(text::SM))
                        .text_color(c.text_3)
                        .child(icon(Icon::GitBranch, c.text_3).size(px(12.)))
                        .child(div().truncate().font_family(MONO_FONT).child(branch)),
                )
            })
            .when_some(status.filter(|s| *s != ThreadStatus::Ready), |this, status| {
                let color = crate::sidebar::status_color(status, cx);
                this.child(pill(status.label(), color, Hsla { a: 0.14, ..color }))
            })
            .child(div().flex_1())
            .child(self.render_tools(cx))
            .into_any_element()
    }

    fn render_item(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(item) = self.items.get(index).cloned() else {
            return div().into_any_element();
        };
        let body = match &item {
            TimelineItem::Message(message) => self.render_message(index, message, cx),
            TimelineItem::Work(group) => self.render_work(index, &item.key(), group, cx),
            TimelineItem::Plan(plan) => self.render_plan(plan, cx),
            TimelineItem::Diff(diff) => self.render_diff(index, diff, cx),
        };
        div()
            .w_full()
            .flex()
            .justify_center()
            .px(px(24.))
            .py(px(6.))
            .child(div().w_full().max_w(px(COLUMN)).child(body))
            .into_any_element()
    }

    fn render_message(&mut self, index: usize, message: &zc_contracts::OrchestrationMessage, cx: &mut Context<Self>) -> AnyElement {
        let c = cx.theme().colors.clone();
        let id = message.id.as_str().to_owned();
        match message.role {
            OrchestrationMessageRole::User => div()
                .flex()
                .justify_end()
                .pt(px(10.))
                .child(
                    div()
                        .max_w(px(COLUMN * 0.82))
                        .px(px(14.))
                        .py(px(10.))
                        .rounded(px(radius::LG))
                        .bg(c.accent_soft)
                        .text_size(px(text::MD))
                        .line_height(px(23.))
                        .text_color(c.text)
                        .child(SharedString::from(message.text.clone()))
                        .when(message.attachments.as_ref().is_some_and(|a| !a.is_empty()), |this| {
                            let count = message.attachments.as_ref().map(|a| a.len()).unwrap_or(0);
                            this.child(
                                div()
                                    .pt(px(6.))
                                    .flex()
                                    .items_center()
                                    .gap(px(4.))
                                    .text_size(px(text::SM))
                                    .text_color(c.text_2)
                                    .child(icon(Icon::Paperclip, c.text_3))
                                    .child(SharedString::from(format!("{count} attachment{}", if count == 1 { "" } else { "s" }))),
                            )
                        }),
                )
                .into_any_element(),
            OrchestrationMessageRole::Reasoning => {
                let key = format!("m:{id}");
                let open = self.expanded.contains(&key);
                let text_copy = message.text.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .id(SharedString::from(format!("reason-{id}")))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .cursor_pointer()
                            .text_size(px(text::SM))
                            .text_color(c.text_3)
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle(key.clone(), index, cx)))
                            .child(icon(Icon::Brain, c.text_3).size(px(13.)))
                            .child(if message.streaming { "Thinking…" } else { "Thought" })
                            .child(icon(if open { Icon::ChevronDown } else { Icon::ChevronRight }, c.text_3).size(px(11.))),
                    )
                    .when(open, |this| {
                        this.child(
                            div()
                                .pl(px(20.))
                                .border_l_2()
                                .border_color(c.line)
                                .child(markdown::render(format!("r{id}"), &text_copy, text::BASE, cx)),
                        )
                    })
                    .into_any_element()
            }
            OrchestrationMessageRole::System => div()
                .flex()
                .items_start()
                .gap(px(8.))
                .text_size(px(text::SM))
                .line_height(px(18.))
                .text_color(c.text_3)
                .child(div().pt(px(2.)).flex_none().child(icon(Icon::Info, c.text_3).size(px(12.))))
                .child(div().flex_1().min_w_0().child(SharedString::from(message.text.clone())))
                .into_any_element(),
            OrchestrationMessageRole::Assistant => {
                let copy = message.text.clone();
                div()
                    .group("assistant")
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(markdown::render(format!("a{id}"), &message.text, text::MD, cx))
                    .when(message.streaming, |this| this.child(div().size(px(8.)).rounded_full().bg(c.accent)))
                    .when(!message.streaming, |this| {
                        this.child(
                            div().flex().invisible().group_hover("assistant", |s| s.visible()).child(
                                Button::new(SharedString::from(format!("copy-{id}")))
                                    .icon(Icon::Copy)
                                    .small()
                                    .tooltip("Copy the answer")
                                    .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))),
                            ),
                        )
                    })
                    .into_any_element()
            }
        }
    }

    fn render_work(&mut self, index: usize, key: &str, group: &WorkGroup, cx: &mut Context<Self>) -> AnyElement {
        let c = cx.theme().colors.clone();
        let open = self.expanded.contains(key) || (group.running && group.entries.len() <= 3);
        let toggle_key = key.to_owned();
        let tool_entries = group.entries.iter().filter(|e| e.is_tool_like()).count();
        let lead_icon = if group.running {
            spinner(SharedString::from(format!("work-spin-{key}")), c.accent, 13.).into_any_element()
        } else if group.failed > 0 {
            icon(Icon::Warning, c.warning).size(px(13.)).into_any_element()
        } else {
            icon(Icon::Wrench, c.text_3).size(px(13.)).into_any_element()
        };
        let single_info = tool_entries == 0 && group.entries.len() == 1;
        if single_info {
            let entry = &group.entries[0];
            return self.render_entry(key, entry, cx);
        }
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(
                div()
                    .id(SharedString::from(format!("work-{key}")))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .py(px(4.))
                    .cursor_pointer()
                    .text_size(px(text::BASE))
                    .text_color(c.text_2)
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle(toggle_key.clone(), index, cx)))
                    .child(lead_icon)
                    .child(div().truncate().child(SharedString::from(group.summary.clone())))
                    .when(group.failed > 0, |this| {
                        this.child(pill(format!("{} failed", group.failed), c.warning, Hsla { a: 0.14, ..c.warning }))
                    })
                    .child(icon(if open { Icon::ChevronDown } else { Icon::ChevronRight }, c.text_3).size(px(11.))),
            )
            .when(open, |this| {
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .ml(px(6.))
                        .pl(px(14.))
                        .border_l_1()
                        .border_color(c.line)
                        .children(group.entries.iter().map(|entry| self.render_entry(key, entry, cx))),
                )
            })
            .into_any_element()
    }

    fn render_entry(&self, group_key: &str, entry: &WorkEntry, cx: &mut Context<Self>) -> AnyElement {
        let c = cx.theme().colors.clone();
        let failed = entry.failed();
        let entry_icon = match entry.action() {
            Action::Read => Icon::FileSearch,
            Action::Edit => Icon::FileEdit,
            Action::Command => Icon::SquareTerminal,
            Action::CodeSearch => Icon::Search,
            Action::WebSearch => Icon::Globe,
            Action::Other => Icon::Wrench,
            Action::Update if failed => Icon::CircleAlert,
            Action::Update => Icon::Info,
        };
        let color = if failed { c.danger } else { c.text_3 };
        let key = format!("{group_key}/{}", entry.id);
        let open = self.expanded.contains(&key);
        let detail = entry.detail.clone().filter(|_| open);
        let has_more = entry.detail.as_ref().is_some_and(|d| d.lines().count() > 1 || d.len() > 100) || !entry.changed_files.is_empty();
        let index = self.items.iter().position(|i| i.key() == group_key).unwrap_or(0);
        let is_tool = entry.is_tool_like();
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .id(SharedString::from(format!("entry-{key}")))
                    .flex()
                    .items_start()
                    .gap(px(8.))
                    .py(px(3.))
                    .min_w_0()
                    .text_size(px(text::SM))
                    .line_height(px(18.))
                    .when(has_more, |this| {
                        let key = key.clone();
                        this.cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle(key.clone(), index, cx)))
                    })
                    .child(div().pt(px(2.)).flex_none().child(if entry.running() {
                        spinner(SharedString::from(format!("entry-spin-{key}")), c.accent, 12.).into_any_element()
                    } else {
                        icon(entry_icon, color).size(px(12.)).into_any_element()
                    }))
                    .child(if is_tool {
                        // A tool: its name, then what it ran or touched, on one line.
                        div()
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(8.))
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(if failed { c.danger } else { c.text_2 })
                                    .child(SharedString::from(entry.label.clone())),
                            )
                            .when_some(entry.preview(), |this, preview| {
                                this.child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .font_family(MONO_FONT)
                                        .text_size(px(text::XS))
                                        .text_color(c.text_3)
                                        .child(SharedString::from(preview)),
                                )
                            })
                            .into_any_element()
                    } else {
                        // A note (progress, a warning): its text, wrapped.
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_color(if failed { c.danger } else { c.text_3 })
                            .child(SharedString::from(worklog::truncate(&entry.label, 600)))
                            .into_any_element()
                    }),
            )
            .when(open && !entry.changed_files.is_empty(), |this| {
                this.child(
                    div()
                        .pl(px(20.))
                        .flex()
                        .flex_col()
                        .font_family(MONO_FONT)
                        .text_size(px(text::XS))
                        .text_color(c.text_2)
                        .children(entry.changed_files.iter().map(|f| SharedString::from(f.clone()))),
                )
            })
            .when_some(detail, |this, detail| {
                let shown: String = detail.lines().take(40).collect::<Vec<_>>().join("\n");
                this.child(
                    div()
                        .ml(px(20.))
                        .mt(px(2.))
                        .mb(px(4.))
                        .px(px(10.))
                        .py(px(8.))
                        .rounded(px(radius::SM))
                        .bg(c.bg_sunken)
                        .font_family(MONO_FONT)
                        .text_size(px(text::XS))
                        .line_height(px(17.))
                        .text_color(c.text_2)
                        .child(SharedString::from(shown)),
                )
            })
            .into_any_element()
    }

    fn render_plan(&mut self, plan: &zc_contracts::OrchestrationProposedPlan, cx: &mut Context<Self>) -> AnyElement {
        let c = cx.theme().colors.clone();
        let actionable = plan.implemented_at.is_none()
            && self.thread(cx).and_then(timeline::actionable_plan).is_some_and(|p| p.id == plan.id)
            && !self.store.read(cx).thread(self.thread_id()).is_some_and(|t| t.is_running());
        let plan_id = plan.id.clone();
        let plan_markdown = plan.plan_markdown.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(16.))
            .rounded(px(radius::LG))
            .border_1()
            .border_color(c.line_strong)
            .bg(c.bg_sunken)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(icon(Icon::ListChecks, c.accent_text))
                    .child(caps_label("Proposed plan", cx))
                    .when(plan.implemented_at.is_some(), |this| {
                        this.child(pill("Implemented", c.success, Hsla { a: 0.14, ..c.success }))
                    }),
            )
            .child(markdown::render(format!("plan{}", plan.id), &plan.plan_markdown, text::BASE, cx))
            .when(actionable, |this| {
                this.child(
                    div().flex().gap(px(8.)).child(
                        Button::new(SharedString::from(format!("implement-{plan_id}")))
                            .label("Implement the plan")
                            .icon(Icon::Play)
                            .variant(Variant::Primary)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let text = timeline::implement_plan_prompt(&plan_markdown);
                                this.send(text, Some(plan_id.clone()), window, cx);
                            })),
                    ),
                )
            })
            .into_any_element()
    }

    fn render_diff(&mut self, index: usize, diff: &zc_contracts::OrchestrationCheckpointSummary, cx: &mut Context<Self>) -> AnyElement {
        let c = cx.theme().colors.clone();
        let key = format!("d:{}", diff.turn_id.as_str());
        let (additions, deletions) = diff.files.iter().fold((0i64, 0i64), |(a, d), f| (a + f.additions, d + f.deletions));
        let open = self.expanded.contains(&key);
        let turn_count: i64 = diff.checkpoint_turn_count;
        let revert_count = turn_count - 1;
        let thread_id = self.thread_id().as_str().to_owned();
        let toggle_key = key.clone();
        div()
            .flex()
            .flex_col()
            .rounded(px(radius::MD))
            .border_1()
            .border_color(c.line)
            .child(
                div()
                    .id(SharedString::from(format!("diff-{key}")))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(12.))
                    .h(px(36.))
                    .cursor_pointer()
                    .text_size(px(text::SM))
                    .on_click(cx.listener(move |this, _, _, cx| this.load_diff(toggle_key.clone(), turn_count, index, cx)))
                    .child(icon(Icon::FileDiff, c.text_3))
                    .child(div().text_color(c.text_2).child(SharedString::from(format!(
                        "Changed {} file{}",
                        diff.files.len(),
                        if diff.files.len() == 1 { "" } else { "s" }
                    ))))
                    .child(
                        div()
                            .font_family(MONO_FONT)
                            .text_color(c.success)
                            .child(SharedString::from(format!("+{additions}"))),
                    )
                    .child(
                        div()
                            .font_family(MONO_FONT)
                            .text_color(c.danger)
                            .child(SharedString::from(format!("−{deletions}"))),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new(SharedString::from(format!("revert-{key}")))
                            .icon(Icon::Revert)
                            .small()
                            .tooltip("Undo this turn and the ones after it (files and conversation)")
                            .on_click(cx.listener(move |_this, _, window, cx| {
                                let answer = window.prompt(
                                    gpui::PromptLevel::Warning,
                                    "Revert to before this turn?",
                                    Some("Files go back to how they were, and the messages after that point are removed."),
                                    &["Revert", "Cancel"],
                                    cx,
                                );
                                let thread_id = thread_id.clone();
                                cx.spawn(async move |this, cx| {
                                    if answer.await == Ok(0) {
                                        let _ = this.update(cx, |this, cx| {
                                            this.command("thread.revert", json!({"threadId": thread_id, "turnCount": revert_count.max(0)}), cx)
                                                .detach()
                                        });
                                    }
                                })
                                .detach();
                            })),
                    )
                    .child(icon(if open { Icon::ChevronDown } else { Icon::ChevronRight }, c.text_3).size(px(11.))),
            )
            .when(open, |this| {
                let content: AnyElement = match self.diffs.get(&key) {
                    None | Some(DiffState::Loading) => div()
                        .p(px(12.))
                        .text_size(px(text::SM))
                        .text_color(c.text_3)
                        .child("Loading the diff…")
                        .into_any_element(),
                    Some(DiffState::Failed(error)) => div()
                        .p(px(12.))
                        .text_size(px(text::SM))
                        .text_color(c.danger)
                        .child(SharedString::from(error.clone()))
                        .into_any_element(),
                    Some(DiffState::Loaded(patch)) => render_patch(&key, patch, cx),
                };
                this.child(div().border_t_1().border_color(c.line).child(content))
            })
            .when(!open, |this| {
                this.child(
                    div()
                        .px(px(12.))
                        .pb(px(8.))
                        .flex()
                        .flex_col()
                        .gap(px(1.))
                        .font_family(MONO_FONT)
                        .text_size(px(text::XS))
                        .children(diff.files.iter().take(8).map(|f| {
                            div()
                                .flex()
                                .gap(px(8.))
                                .child(div().flex_1().truncate().text_color(c.text_2).child(SharedString::from(f.path.clone())))
                                .child(div().text_color(c.success).child(SharedString::from(format!("+{}", f.additions))))
                                .child(div().text_color(c.danger).child(SharedString::from(format!("−{}", f.deletions))))
                        }))
                        .when(diff.files.len() > 8, |this| {
                            this.child(
                                div()
                                    .text_color(c.text_3)
                                    .child(SharedString::from(format!("and {} more", diff.files.len() - 8))),
                            )
                        }),
                )
            })
            .into_any_element()
    }

    fn render_requests(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let thread = self.thread(cx)?;
        let pending = pending_requests(&thread.activities);
        if pending.is_empty() {
            return None;
        }
        let c = cx.theme().colors.clone();
        let mut cards: Vec<AnyElement> = Vec::new();
        for approval in &pending.approvals {
            let approval_for_buttons = approval.clone();
            cards.push(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .gap(px(10.))
                    .p(px(14.))
                    .rounded(px(radius::LG))
                    .border_1()
                    .border_color(c.warning)
                    .bg(Hsla { a: 0.08, ..c.warning })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .text_size(px(text::BASE))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(icon(Icon::Hand, c.warning))
                            .child(approval.title())
                            .when_some(approval.app_name.clone(), |this, app| this.child(pill(app, c.text_2, c.hover))),
                    )
                    .when_some(approval.detail.clone(), |this, detail| {
                        this.child(
                            div()
                                .px(px(10.))
                                .py(px(8.))
                                .rounded(px(radius::SM))
                                .bg(c.bg_sunken)
                                .font_family(MONO_FONT)
                                .text_size(px(text::SM))
                                .line_height(px(19.))
                                .text_color(c.text)
                                .child(SharedString::from(detail.lines().take(16).collect::<Vec<_>>().join("\n"))),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(px(8.))
                            .children(approval_for_buttons.choices().into_iter().map(|choice| {
                                let approval = approval_for_buttons.clone();
                                let decision = choice.decision.clone();
                                let variant = match decision.as_str() {
                                    "accept" => Variant::Primary,
                                    "decline" | "cancel" => Variant::Secondary,
                                    _ => Variant::Secondary,
                                };
                                Button::new(SharedString::from(format!("approve-{}-{}", approval.request_id, decision)))
                                    .label(choice.label.clone())
                                    .variant(variant)
                                    .on_click(cx.listener(move |this, _, _, cx| this.respond_approval(&approval, &decision, cx)))
                            })),
                    )
                    .into_any_element(),
            );
        }
        for input in &pending.user_inputs {
            let input_copy = input.clone();
            let complete = input.questions.iter().all(|q| {
                self.answers
                    .get(&format!("{}:{}", input.request_id, q.id))
                    .is_some_and(|a| !a.selected.is_empty())
            });
            cards.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .p(px(14.))
                    .rounded(px(radius::LG))
                    .border_1()
                    .border_color(c.accent)
                    .bg(c.accent_soft)
                    .children(input.questions.iter().map(|question| {
                        let key = format!("{}:{}", input.request_id, question.id);
                        let selected = self.answers.get(&key).map(|a| a.selected.clone()).unwrap_or_default();
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .when(!question.header.is_empty(), |this| this.child(caps_label(question.header.clone(), cx)))
                            .child(
                                div()
                                    .text_size(px(text::MD))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(SharedString::from(question.question.clone())),
                            )
                            .child(div().flex().flex_col().gap(px(6.)).children(question.options.iter().map(|option| {
                                let chosen = selected.contains(&option.label);
                                let key = key.clone();
                                let label = option.label.clone();
                                let multi = question.multi_select;
                                div()
                                    .id(SharedString::from(format!("opt-{key}-{label}")))
                                    .flex()
                                    .items_start()
                                    .gap(px(10.))
                                    .px(px(10.))
                                    .py(px(8.))
                                    .rounded(px(radius::SM))
                                    .border_1()
                                    .border_color(if chosen { c.accent } else { c.line_strong })
                                    .when(chosen, |this| this.bg(c.accent_soft))
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let answer = this.answers.entry(key.clone()).or_default();
                                        if multi {
                                            if let Some(i) = answer.selected.iter().position(|s| *s == label) {
                                                answer.selected.remove(i);
                                            } else {
                                                answer.selected.push(label.clone());
                                            }
                                        } else {
                                            answer.selected = vec![label.clone()];
                                        }
                                        cx.notify();
                                    }))
                                    .child(icon(
                                        if chosen { Icon::CircleCheck } else { Icon::CircleDot },
                                        if chosen { c.accent_text } else { c.text_3 },
                                    ))
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .child(
                                                div()
                                                    .text_size(px(text::BASE))
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .child(SharedString::from(option.label.clone())),
                                            )
                                            .when_some(option.description.clone(), |this, d| {
                                                this.child(div().text_size(px(text::SM)).text_color(c.text_2).child(SharedString::from(d)))
                                            }),
                                    )
                            })))
                    }))
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(
                                Button::new(SharedString::from(format!("answer-{}", input.request_id)))
                                    .label("Answer")
                                    .variant(Variant::Primary)
                                    .disabled(!complete)
                                    .on_click(cx.listener(move |this, _, _, cx| this.submit_answers(&input_copy, cx))),
                            )
                            .when(input.dismissible, |this| {
                                let request_id = input.request_id.clone();
                                this.child(
                                    Button::new(SharedString::from(format!("dismiss-{}", input.request_id)))
                                        .label("Dismiss")
                                        .variant(Variant::Secondary)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            let params = json!({"threadId": this.thread_id().as_str(), "requestId": request_id});
                                            this.command("thread.dismissQuestion", params, cx).detach();
                                        })),
                                )
                            }),
                    )
                    .into_any_element(),
            );
        }
        Some(div().flex().flex_col().min_w_0().gap(px(10.)).children(cards).into_any_element())
    }

    /// "Working for 1m 20s", the agent's plan, or the last error, above the composer.
    fn render_status_line(&self, cx: &App) -> Option<AnyElement> {
        let c = cx.theme().colors.clone();
        let thread = self.thread(cx)?;
        let running = thread.session.as_ref().is_some_and(|s| matches!(s.status.as_str(), "running" | "starting"));
        let current_turn = thread.latest_turn.as_ref().map(|t| t.turn_id.as_str().to_owned());
        let plan = worklog::active_plan(&thread.activities, current_turn.as_deref()).filter(|_| running);
        if running {
            let since = thread
                .latest_turn
                .as_ref()
                .and_then(|t| t.started_at.as_deref().or(Some(t.requested_at.as_str())))
                .and_then(millis);
            let label = match since {
                Some(since) => format!("Working for {}", duration(now_millis() - since)),
                None => "Working…".into(),
            };
            return Some(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_size(px(text::SM))
                    .text_color(c.text_2)
                    .child(spinner("working", c.accent, 13.))
                    .child(SharedString::from(label))
                    .when_some(plan, |this, plan| {
                        let step = plan.steps.iter().find(|s| s.status == "inProgress").map(|s| s.step.clone());
                        this.child(pill(format!("{}/{}", plan.completed(), plan.steps.len()), c.accent_text, c.accent_soft))
                            .when_some(step, |this, step| {
                                this.child(div().truncate().text_color(c.text_3).child(SharedString::from(step)))
                            })
                    })
                    .into_any_element(),
            );
        }
        let error = thread
            .session
            .as_ref()
            .filter(|s| s.status.as_str() == "error")
            .and_then(|s| s.last_error.clone());
        error.map(|error| {
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .text_size(px(text::SM))
                .text_color(c.danger)
                .child(icon(Icon::CircleAlert, c.danger))
                .child(div().truncate().child(SharedString::from(error)))
                .into_any_element()
        })
    }
}

/// A unified diff, colored per line, in the mono font.
fn render_patch(key: &str, patch: &str, cx: &App) -> AnyElement {
    let c = cx.theme().colors.clone();
    if patch.trim().is_empty() {
        return div()
            .p(px(12.))
            .text_size(px(text::SM))
            .text_color(c.text_3)
            .child("No textual change.")
            .into_any_element();
    }
    let lines: Vec<&str> = patch.lines().take(4000).collect();
    let truncated = patch.lines().count() > lines.len();
    div()
        .id(SharedString::from(format!("patch-{key}")))
        .overflow_x_scroll()
        .py(px(6.))
        .font_family(MONO_FONT)
        .text_size(px(text::XS))
        .line_height(px(17.))
        .child(
            div()
                .flex()
                .flex_col()
                .min_w_full()
                .children(lines.into_iter().map(|line| {
                    let (fg, bg) = if line.starts_with("+++") || line.starts_with("---") || line.starts_with("diff ") || line.starts_with("index ") {
                        (c.text, Some(c.hover))
                    } else if line.starts_with("@@") {
                        (c.accent_text, None)
                    } else if line.starts_with('+') {
                        (c.text, Some(Hsla { a: 0.14, ..c.success }))
                    } else if line.starts_with('-') {
                        (c.text, Some(Hsla { a: 0.14, ..c.danger }))
                    } else {
                        (c.text_2, None)
                    };
                    div()
                        .px(px(12.))
                        .whitespace_nowrap()
                        .text_color(fg)
                        .when_some(bg, |this, bg| this.bg(bg))
                        .child(SharedString::from(if line.is_empty() { " ".to_owned() } else { line.replace('\t', "    ") }))
                }))
                .when(truncated, |this| this.child(div().px(px(12.)).text_color(c.text_3).child("… (diff truncated)"))),
        )
        .into_any_element()
}

impl Render for ThreadView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = cx.theme().colors.clone();
        let draft = self.is_draft();
        let loaded = self.store.read(cx).thread(self.thread_id()).is_some_and(|t| t.loaded);
        let requests = self.render_requests(cx);
        let terminals = self.render_terminals(cx);
        let status_line = self.render_status_line(cx);
        let project_title = match &self.target {
            Target::Draft { project, .. } => self.store.read(cx).shell.project(project).map(|p| p.title.clone()),
            _ => None,
        };
        let view = cx.entity().downgrade();
        let pending = self.pending_send.clone();

        let body: AnyElement = if draft && self.pending_send.is_none() {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.))
                .px(px(24.))
                .child(
                    div()
                        .text_size(px(text::XXL))
                        .font_weight(FontWeight::BOLD)
                        .text_color(c.text)
                        .child("What should we build?"),
                )
                .child(
                    div()
                        .text_size(px(text::BASE))
                        .text_color(c.text_2)
                        .child(SharedString::from(match project_title {
                            Some(p) => format!("A new thread in {p}. Pick the model and how much it may do on its own below."),
                            None => "A new thread.".to_owned(),
                        })),
                )
                .into_any_element()
        } else if !loaded && !draft {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .gap(px(8.))
                .text_color(c.text_3)
                .child(spinner("thread-loading", c.text_3, 14.))
                .child("Loading the thread…")
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_h_0()
                .child(
                    list(self.list.clone(), move |index, _, cx| {
                        view.update(cx, |this, cx| this.render_item(index, cx))
                            .unwrap_or_else(|_| div().into_any_element())
                    })
                    .size_full(),
                )
                .into_any_element()
        };

        let this = self.this.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .relative()
            .child(
                canvas(
                    move |bounds, _, cx| {
                        let width = (bounds.size.width - px(48.)).max(px(200.));
                        this.update(cx, |this, cx| {
                            if this.width != Some(width) {
                                this.width = Some(width);
                                cx.notify();
                            }
                        })
                        .ok();
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(body)
            .when_some(pending.filter(|_| draft || loaded), |this, text| {
                // The message as sent, until the server's copy shows in the timeline.
                this.when(draft, |this| {
                    this.child(
                        div().flex().justify_center().px(px(24.)).child(
                            div().w_full().max_w(px(COLUMN)).flex().justify_end().child(
                                div()
                                    .px(px(14.))
                                    .py(px(10.))
                                    .rounded(px(radius::LG))
                                    .bg(c.accent_soft)
                                    .opacity(0.7)
                                    .child(SharedString::from(text)),
                            ),
                        ),
                    )
                })
            })
            .child(
                // The column gets a definite width (the view's, from the last frame): wrapped
                // text in it is then measured as it is painted, and the area is as tall as
                // what it shows.
                div().flex().flex_col().px(px(24.)).pt(px(8.)).pb(px(20.)).child(
                    div()
                        .map(|this| match self.width {
                            Some(width) => this.w(width.min(px(COLUMN))),
                            None => this.w_full().max_w(px(COLUMN)),
                        })
                        .mx_auto()
                        .flex()
                        .flex_col()
                        .gap(px(10.))
                        .when_some(requests, |this, requests| {
                            this.child(div().id("thread-requests").min_w_0().max_h(px(420.)).overflow_y_scroll().child(requests))
                        })
                        .when_some(status_line, |this, line| this.child(line))
                        .child(self.composer.clone()),
                ),
            )
            .when_some(terminals, |this, terminals| this.child(terminals))
            .when_some(self.menu.as_ref(), |this, menu| this.child(menu.render()))
    }
}
