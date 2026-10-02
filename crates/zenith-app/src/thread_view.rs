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
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    canvas, div, list, px, relative, svg, AnyElement, App, ClipboardItem, Context, Corner, Entity, EventEmitter, Focusable, FontWeight, Hsla, ListAlignment,
    ListState, MouseButton, MouseDownEvent, Pixels, Point, SharedString, Subscription, WeakEntity, Window,
};
use serde_json::{json, Value};
use zc_contracts::{OrchestrationMessageRole, OrchestrationThread, OrchestrationThreadShell, ProjectId, ThreadId};
use zenith_model::git;
use zenith_model::requests::{pending_requests, PendingApproval, PendingUserInput};
use zenith_model::rows::{self, Entry as RowEntry, Expanded, Row, SummaryKind};
use zenith_model::time::{format_duration, millis, now_millis};
use zenith_model::timeline;
use zenith_model::worklog::{self, WorkEntry};

use crate::assets::{Icon, MONO_FONT};
use crate::composer::{Composer, ComposerContext, ComposerEvent, EnvMode, ModelChoice};
use crate::store::{self, Store, StoreEvent};
use crate::terminal::{TermStatus, TerminalPanel};
use crate::theme::{radius, text, ActiveTheme};
use crate::ui::badges::project_badge;
use crate::ui::controls::{header_toggle, outline_button, split_separator, Part};
use crate::ui::markdown::MdStyle;
use crate::ui::menu::{Entry, OpenMenu};
use crate::ui::OneLine;
use crate::ui::{caps_label, icon, markdown, pill, spinner, Button, Tooltip, Variant};

/// The timeline's reading width.
/// The web's `--chat-max-width` (48rem).
const COLUMN: f32 = 768.;

pub enum ThreadViewEvent {
    /// The draft's first message created this thread.
    Created(ThreadId),
    /// "Rename thread" from the title's menu: renamed where the sidebar shows it.
    Rename,
    /// Archived or deleted from the title's menu.
    Removed,
    /// "Add action": a new script for this project.
    AddAction(ProjectId),
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
    rows: Vec<Row>,
    /// The folds and groups opened ("Worked for…", "Ran 3 commands").
    open: Expanded,
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
            open: Expanded::default(),
            rows: Vec::new(),
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

    /// The composer starts from the thread's model and modes, once they are known; what is
    /// around it (strip, banner, placeholder) follows the thread.
    fn sync_composer(&mut self, cx: &mut Context<Self>) {
        let running = self.store.read(cx).thread(self.thread_id()).is_some_and(|t| t.is_running());
        let shell_thread = self.store.read(cx).shell.thread(self.thread_id()).cloned();
        let session = self.thread(cx).and_then(|t| t.session.as_ref().map(|s| s.status));
        // The web's phase: no session, or a stopped one, is "disconnected".
        let disconnected = matches!(
            session,
            None | Some(
                zc_contracts::OrchestrationSessionStatus::Stopped
                    | zc_contracts::OrchestrationSessionStatus::Interrupted
                    | zc_contracts::OrchestrationSessionStatus::Error
            )
        );
        let git_ref = self.git.as_ref().and_then(|g| g.get("refName")).and_then(Value::as_str).map(String::from);
        let context = ComposerContext {
            draft: self.is_draft(),
            // A new thread starts from the checkout's branch.
            branch: shell_thread.as_ref().and_then(|t| t.branch.clone()).or(git_ref),
            worktree: shell_thread.as_ref().is_some_and(|t| t.worktree_path.is_some()),
            pull_request: shell_thread.as_ref().and_then(zenith_model::shell::pull_request_badge),
            monitoring: shell_thread
                .as_ref()
                .is_some_and(|t| zenith_model::shell::status(t) == zenith_model::shell::ThreadStatus::Monitoring),
            settled: shell_thread
                .as_ref()
                .is_some_and(|t| zenith_model::shell::section(t, now_millis()) == zenith_model::shell::Section::Settled),
            snoozed: shell_thread
                .as_ref()
                .is_some_and(|t| zenith_model::shell::section(t, now_millis()) == zenith_model::shell::Section::Snoozed),
        };
        let placeholder = if disconnected {
            "Ask for changes, send follow-ups, or attach images"
        } else {
            "Ask anything, @tag files/folders, $use skills, or / for commands"
        };
        self.composer.update(cx, |c, cx| {
            c.running = running;
            c.context = context;
            c.editor.update(cx, |e, cx| e.set_placeholder(placeholder, cx));
            cx.notify();
        });
        if self.composer_synced || self.is_draft() {
            return;
        }
        let Some(thread) = self.thread(cx).cloned() else { return };
        self.composer_synced = true;
        let model = serde_json::to_value(&thread.model_selection).ok().and_then(|v| ModelChoice::from_json(&v));
        self.composer.update(cx, |c, cx| {
            c.model = model;
            c.runtime_mode = thread.runtime_mode.as_str().to_owned();
            c.plan_mode = thread.interaction_mode.as_str() == "plan";
            cx.notify();
        });
    }

    fn fingerprint(row: &Row) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        row.id().hash(&mut h);
        match row {
            Row::Message { message, show_meta, diff, .. } => {
                message.text.len().hash(&mut h);
                message.streaming.hash(&mut h);
                message.updated_at.hash(&mut h);
                show_meta.hash(&mut h);
                diff.as_ref().map(|d| d.files.len()).hash(&mut h);
            }
            Row::AssistantMeta { message, .. } => message.updated_at.hash(&mut h),
            Row::TurnFold { label, expanded, .. } => {
                label.hash(&mut h);
                expanded.hash(&mut h);
            }
            Row::ActivityGroup { entries, expanded, active, .. } => {
                entries.len().hash(&mut h);
                expanded.hash(&mut h);
                active.hash(&mut h);
                for entry in entries {
                    if let RowEntry::Message(m) = entry {
                        m.text.len().hash(&mut h);
                    }
                }
            }
            Row::Work { entries, label, .. } => {
                label.hash(&mut h);
                for e in entries {
                    e.id.hash(&mut h);
                    format!("{:?}", e.status).hash(&mut h);
                    e.detail.as_ref().map(String::len).hash(&mut h);
                }
            }
            Row::WorkLive {
                entry,
                entries,
                expanded,
                active,
                ..
            } => {
                entry.id.hash(&mut h);
                format!("{:?}", entry.status).hash(&mut h);
                entries.len().hash(&mut h);
                expanded.hash(&mut h);
                active.hash(&mut h);
            }
            Row::WorkToggle { summary, expanded, failed, .. } => {
                summary.hash(&mut h);
                expanded.hash(&mut h);
                failed.hash(&mut h);
            }
            Row::Compaction { label, .. } => label.hash(&mut h),
            Row::Plan { plan, .. } => {
                plan.updated_at.hash(&mut h);
                plan.implemented_at.hash(&mut h);
            }
            Row::Working { since } => since.hash(&mut h),
            Row::Thinking => {}
        }
        h.finish()
    }

    /// Rebuilds the timeline, remeasuring only what changed.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let Some(thread) = self.thread(cx) else {
            cx.notify();
            return;
        };
        let items = rows::rows(thread, &self.open);
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
        self.rows = items;
        self.fingerprints = fingerprints;
        self.sync_composer(cx);
        cx.notify();
    }

    fn remeasure(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.rows.len() {
            self.list.splice(index..index + 1, 1);
        }
        cx.notify();
    }

    /// Opens or closes a fold ("Worked for…") or a group ("Ran 3 commands"): the rows change.
    fn toggle_turn(&mut self, turn: String, cx: &mut Context<Self>) {
        if !self.open.turns.remove(&turn) {
            self.open.turns.insert(turn);
        }
        self.rebuild(cx);
    }

    fn toggle_group(&mut self, group: String, cx: &mut Context<Self>) {
        if !self.open.groups.remove(&group) {
            self.open.groups.insert(group);
        }
        self.rebuild(cx);
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
            ComposerEvent::Command(name) => {
                let params = json!({"threadId": self.thread_id().as_str()});
                self.command(name, params, cx).detach();
            }
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
                let index = index.min(this.rows.len().saturating_sub(1));
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
                    // The strip under the composer shows the checkout's branch.
                    this.sync_composer(cx);
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
    /// The header's actions (`ChatHeader`'s cluster): the project's script ("Add action" while
    /// it has none), "Open" in an editor (this machine's server only), the git action and its
    /// menu, then the panel toggles.
    fn render_tools(&self, cx: &App) -> AnyElement {
        let draft = self.is_draft();
        let store = self.store.read(cx);
        let scripts = self
            .project_id(cx)
            .and_then(|p| store.shell.project(&p).map(|p| p.scripts.clone()))
            .unwrap_or_default();
        let editors = store.config.as_ref().map(|c| c.available_editors.len()).unwrap_or(0);
        let local = !store.is_remote();
        let git = self.git.as_ref().filter(|g| g.get("isRepo").and_then(Value::as_bool).unwrap_or(false));
        let quick = git::quick_action(git, self.git_busy.is_some());

        let this = self.this.clone();
        // The primary script (`primaryProjectScript`): the first that is not a worktree's setup.
        let script_control = match scripts.iter().find(|s| !s.run_on_worktree_create) {
            None => outline_button("add-action", Some(Icon::Plus), Some("Add action".into()), Part::Whole, false, cx)
                .tooltip(|_, cx| Tooltip::view("Add action", None, cx))
                .on_click(move |_, window, cx| {
                    this.update(cx, |v, cx| v.open_script_editor(window, cx)).ok();
                })
                .into_any_element(),
            Some(primary) => {
                let id = primary.id.to_string();
                let name = primary.name.to_string();
                let this_menu = this.clone();
                div()
                    .flex()
                    .flex_none()
                    .child(
                        outline_button("run-script", Some(script_icon(primary.icon)), Some(name.clone().into()), Part::Main, draft, cx)
                            .tooltip(move |_, cx| Tooltip::view(format!("Run {name}"), None, cx))
                            .on_click(move |_, window, cx| {
                                this.update(cx, |v, cx| v.run_script(id.clone(), window, cx)).ok();
                            }),
                    )
                    .child(split_separator(cx))
                    .child(
                        outline_button("script-actions", None, None, Part::Chevron, false, cx).on_click(move |event, window, cx| {
                            this_menu.update(cx, |v, cx| v.open_scripts_menu(event.position(), window, cx)).ok();
                        }),
                    )
                    .into_any_element()
            }
        };

        let open = local.then(|| {
            let this = self.this.clone();
            let disabled = editors == 0 || draft;
            div()
                .flex()
                .flex_none()
                .child(
                    outline_button("open-editor", None, Some("Open".into()), Part::Main, disabled, cx)
                        .pl(px(9.))
                        .when(!disabled, |el| {
                            el.on_click(move |_, _, cx| {
                                this.update(cx, |v, cx| v.open_in_editor(None, cx)).ok();
                            })
                        }),
                )
                .child(split_separator(cx))
                .child({
                    let this = self.this.clone();
                    outline_button("choose-editor", None, None, Part::Chevron, false, cx).on_click(move |event, window, cx| {
                        this.update(cx, |v, cx| v.open_editors_menu(event.position(), window, cx)).ok();
                    })
                })
        });

        let git_control = git.is_some().then(|| {
            let this = self.this.clone();
            let disabled = quick.disabled();
            let hint = match &quick.kind {
                git::QuickKind::Hint(hint) => Some(hint.clone()),
                _ => None,
            };
            let kind = quick.kind.clone();
            let pr_url = git.and_then(|g| g.pointer("/pr/url")).and_then(Value::as_str).map(str::to_owned);
            let this_menu = self.this.clone();
            div()
                .flex()
                .flex_none()
                .child(
                    outline_button(
                        "git-quick",
                        Some(quick_icon(&quick)),
                        Some(quick.label.clone().into()),
                        Part::Main,
                        disabled,
                        cx,
                    )
                    .when_some(hint, |el, hint| el.tooltip(move |_, cx| Tooltip::view(hint.clone(), None, cx)))
                    .when(!disabled, |el| {
                        el.on_click(move |_, _, cx| {
                            let kind = kind.clone();
                            let pr_url = pr_url.clone();
                            this.update(cx, |v, cx| match kind {
                                git::QuickKind::Run(action) => v.git_action(if action == "create_pr" { "pr" } else { action }, cx),
                                git::QuickKind::Pull => v.git_action("pull", cx),
                                git::QuickKind::OpenPr => {
                                    if let Some(url) = pr_url {
                                        cx.open_url(&url);
                                    }
                                }
                                git::QuickKind::Publish => v.git_action("push", cx),
                                git::QuickKind::Hint(_) => {}
                            })
                            .ok();
                        })
                    }),
                )
                .child(split_separator(cx))
                .child(
                    outline_button("git-menu", None, None, Part::Chevron, self.git_busy.is_some(), cx).on_click(move |event, window, cx| {
                        this_menu.update(cx, |v, cx| v.open_git_menu(event.position(), window, cx)).ok();
                    }),
                )
        });

        let this_terminal = self.this.clone();
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(12.))
            .child(script_control)
            .children(open)
            .children(git_control)
            // The toggles end 13 px from the window's edge (the header's padding is 20).
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .ml(px(-8.))
                    .mr(px(-7.))
                    .child(
                        header_toggle("toggle-terminal", Icon::PanelBottom, self.terminal_visible, cx)
                            .tooltip(|_, cx| Tooltip::view("Toggle terminal drawer", Some("⌘J".into()), cx))
                            .when(!draft, |el| {
                                el.on_click(move |_, window, cx| {
                                    this_terminal.update(cx, |v, cx| v.toggle_terminal(window, cx)).ok();
                                })
                            }),
                    )
                    // The web's right panel (changes, previews) comes later; its place is kept.
                    .child(div().size(px(28.)).flex_none()),
            )
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

    /// The title bar's content for this thread, as the web's `ChatHeader`: the project and
    /// the title, then the project's action, "Open", the git action and the terminal toggle.
    pub fn render_header(&self, cx: &App) -> AnyElement {
        let c = cx.theme().colors.clone();
        let store = self.store.read(cx);
        let (title, project, thread): (SharedString, Option<(ProjectId, String)>, Option<OrchestrationThreadShell>) = match &self.target {
            Target::Draft { project, .. } => ("New thread".into(), store.shell.project(project).map(|p| (p.id.clone(), p.title.clone())), None),
            Target::Thread(id) => match store.shell.thread(id) {
                Some(t) => (
                    t.title.clone().into(),
                    store.shell.project(&t.project_id).map(|p| (p.id.clone(), p.title.clone())),
                    Some(t.clone()),
                ),
                None => ("Loading…".into(), None, None),
            },
        };
        let this = self.this.clone();
        let breadcrumb = div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(12.))
            .text_size(px(14.))
            .line_height(px(20.))
            .font_weight(FontWeight::MEDIUM)
            .when_some(project, |this, (_, name)| {
                let text_2 = c.text_2;
                let text = c.text;
                this.child(
                    div()
                        .id("crumb-project")
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(px(6.))
                        .max_w(px(180.))
                        .cursor_pointer()
                        .text_color(text_2)
                        .hover(move |s| s.text_color(text))
                        .tooltip({
                            let name = name.clone();
                            move |_, cx| Tooltip::view(format!("New thread in {name}"), None, cx)
                        })
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(crate::actions::NewThread), cx))
                        .child(project_badge(&name, 14., cx))
                        .child(div().max_w(px(160.)).one_line().child(SharedString::from(name))),
                )
                .child(div().flex_none().font_weight(FontWeight::NORMAL).text_color(c.text_3).child("/"))
            })
            .child(
                div()
                    .id("crumb-title")
                    .group("thread-title")
                    .flex_1()
                    .min_w(px(40.))
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .text_color(c.text)
                    .when_some(thread, |el, thread| {
                        el.cursor_pointer().on_mouse_down(MouseButton::Left, move |event: &MouseDownEvent, window, cx| {
                            let at = event.position;
                            this.update(cx, |v, cx| v.open_thread_menu(&thread, at, window, cx)).ok();
                        })
                    })
                    .child(div().min_w_0().one_line().child(title))
                    .when(!self.is_draft(), |this| {
                        this.child(
                            svg()
                                .path(Icon::ChevronDown.path())
                                .size(px(14.))
                                .flex_none()
                                .text_color(c.text_2)
                                .invisible()
                                .group_hover("thread-title", |s| s.visible()),
                        )
                    }),
            );
        div()
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap(px(12.))
            .child(breadcrumb)
            .child(self.render_tools(cx))
            .into_any_element()
    }

    fn open_script_editor(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(project) = self.project_id(cx) {
            cx.emit(ThreadViewEvent::AddAction(project));
        }
    }

    /// Opens the thread's folder in an editor of the server's machine (`project.openInEditor`).
    fn open_in_editor(&mut self, editor: Option<String>, cx: &mut Context<Self>) {
        if self.is_draft() {
            return;
        }
        let mut params = json!({"threadId": self.thread_id().as_str()});
        if let Some(editor) = editor {
            params["editor"] = json!(editor);
        }
        self.command("project.openInEditor", params, cx).detach();
    }

    fn open_editors_menu(&mut self, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let editors: Vec<String> = self
            .store
            .read(cx)
            .config
            .as_ref()
            .map(|c| c.available_editors.iter().map(|e| e.as_str().to_owned()).collect())
            .unwrap_or_default();
        let entries = if editors.is_empty() {
            vec![Entry::item("No installed editors found", |_, _| {}).disabled(true)]
        } else {
            editors
                .into_iter()
                .enumerate()
                .map(|(index, editor)| {
                    let this = self.this.clone();
                    let label = editor_name(&editor);
                    let entry = Entry::item(label, move |_, cx| {
                        this.update(cx, |v, cx| v.open_in_editor(Some(editor.clone()), cx)).ok();
                    })
                    .icon(Icon::SquareArrowOutUpRight);
                    if index == 0 {
                        entry.keys("⌘O")
                    } else {
                        entry
                    }
                })
                .collect()
        };
        self.open_menu(entries, position, window, cx);
    }

    /// The thread's menu, from its title (the same as its row's in the sidebar).
    fn open_thread_menu(&mut self, thread: &OrchestrationThreadShell, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let this = self.this.clone();
        let rename = Rc::new(move |_: &mut Window, cx: &mut App| {
            this.update(cx, |_, cx| cx.emit(ThreadViewEvent::Rename)).ok();
        }) as crate::sidebar::RenameThread;
        let this = self.this.clone();
        let removed = Rc::new(move |cx: &mut App| {
            this.update(cx, |_, cx| cx.emit(ThreadViewEvent::Removed)).ok();
        }) as crate::sidebar::ThreadRemoved;
        let entries = crate::sidebar::thread_menu_entries(thread, cx, rename, removed);
        let menu = OpenMenu::new(entries, position + gpui::point(px(0.), px(4.)), window, cx, |this, _, _| this.menu = None);
        self.menu = Some(menu);
        cx.notify();
    }

    /// One row of the timeline, in the web's 768 px column with its own space under it.
    fn render_item(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.rows.get(index).cloned() else {
            return div().into_any_element();
        };
        let body = match &row {
            Row::Message { message, show_meta, diff, .. } => self.render_message(index, message, *show_meta, diff.as_ref(), cx),
            Row::AssistantMeta { message, .. } => div().px(px(4.)).mt(px(2.)).child(self.render_meta(message, cx)).into_any_element(),
            Row::TurnFold { turn_id, label, expanded, .. } => self.render_fold(turn_id, label, *expanded, cx),
            Row::ActivityGroup {
                group_id,
                entries,
                expanded,
                active,
                ..
            } => self.render_activity(index, group_id, entries, *expanded, *active, cx),
            Row::Work {
                id,
                entries,
                expanded_group,
                label,
            } => self.render_work_entries(index, id, entries, *expanded_group, label.as_deref(), cx),
            Row::WorkLive {
                entry,
                group_id,
                expanded,
                active,
                ..
            } => {
                let label = entry.live_label(self.workspace_root(cx).as_deref(), *active);
                let group = group_id.clone();
                live_row(work_icon(entry), label.into(), cx)
                    .id(SharedString::from(format!("live-{group_id}")))
                    .cursor_pointer()
                    .when(*expanded, |el| el.bg(cx.theme().colors.accent_soft.opacity(0.2)))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_group(group.clone(), cx)))
                    .into_any_element()
            }
            Row::WorkToggle {
                group_id,
                summary,
                kind,
                expanded,
                failed,
                ..
            } => self.render_toggle(group_id, summary, *kind, *expanded, *failed, cx),
            Row::Compaction { label, .. } => render_compaction(label, cx),
            Row::Plan { plan, .. } => self.render_plan(plan, cx),
            Row::Working { since } => self.render_working(since.as_deref(), cx),
            Row::Thinking => div().min_h(px(28.)).child(live_row(Icon::Brain, "Thinking".into(), cx)).into_any_element(),
        };
        let last = index + 1 == self.rows.len();
        div()
            .w_full()
            .flex()
            .justify_center()
            .px(px(20.))
            // The list's own header and footer (`h-4`).
            .when(index == 0, |el| el.pt(px(16.)))
            .child(
                div()
                    .w_full()
                    .max_w(px(COLUMN))
                    .min_w_0()
                    .pb(px(row.bottom_padding() + if last { 16. } else { 0. }))
                    .child(body),
            )
            .into_any_element()
    }

    fn workspace_root(&self, cx: &App) -> Option<String> {
        let store = self.store.read(cx);
        let thread = store.shell.thread(self.thread_id())?;
        thread
            .worktree_path
            .clone()
            .or_else(|| store.shell.project(&thread.project_id).map(|p| p.workspace_root.clone()))
    }

    fn render_message(
        &mut self,
        index: usize,
        message: &zc_contracts::OrchestrationMessage,
        show_meta: bool,
        diff: Option<&zc_contracts::OrchestrationCheckpointSummary>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = cx.theme().colors.clone();
        let id = message.id.as_str().to_owned();
        match message.role {
            OrchestrationMessageRole::User => {
                // Long messages fold to 176 px with a fade (`UserTimelineRow`).
                let long = message.text.chars().count() > 600 || message.text.lines().count() > 8;
                let key = format!("msg:{id}");
                let open = self.expanded.contains(&key);
                let collapsed = long && !open;
                let fade_to = over(c.message_surface, c.bg_raised);
                // Soft line breaks stay breaks, as on the web (`lineBreaks`).
                let text = message.text.replace('\n', "  \n");
                let attachments = message.attachments.as_ref().map(|a| a.len()).unwrap_or(0);
                div()
                    .group("user-message")
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap(px(4.))
                    .child(
                        div()
                            .max_w(relative(0.8))
                            .rounded(px(radius::XXL))
                            .bg(c.message_surface)
                            .p(px(12.))
                            .when(attachments > 0, |el| {
                                el.child(
                                    div()
                                        .mb(px(8.))
                                        .flex()
                                        .items_center()
                                        .gap(px(4.))
                                        .text_size(px(12.))
                                        .text_color(c.text_2)
                                        .child(icon(Icon::Paperclip, c.text_3))
                                        .child(SharedString::from(format!(
                                            "{attachments} attachment{}",
                                            if attachments == 1 { "" } else { "s" }
                                        ))),
                                )
                            })
                            .child(
                                div()
                                    .relative()
                                    .when(collapsed, |el| el.max_h(px(176.)).overflow_hidden())
                                    .child(markdown::render_styled(format!("u{id}"), &text, MdStyle::web(c.text), cx))
                                    .when(collapsed, |el| {
                                        el.child(div().absolute().left_0().right_0().bottom_0().h(px(28.)).bg(gpui::linear_gradient(
                                            180.,
                                            gpui::linear_color_stop(fade_to.opacity(0.), 0.),
                                            gpui::linear_color_stop(fade_to, 1.),
                                        )))
                                    }),
                            )
                            .when(long, |el| {
                                let key = key.clone();
                                el.child(
                                    div().mt(px(6.)).flex().justify_end().child(
                                        ghost_xs(
                                            SharedString::from(format!("more-{id}")),
                                            if open { "Show less" } else { "Show full message" },
                                            cx,
                                        )
                                        .ml(px(-4.))
                                        .on_click(cx.listener(move |this, _, _, cx| this.toggle(key.clone(), index, cx))),
                                    ),
                                )
                            }),
                    )
                    // The time and actions show on hover; their place is kept.
                    .child(
                        div()
                            .w_full()
                            .max_w(relative(0.8))
                            .h(px(24.))
                            .pr(px(4.))
                            .flex()
                            .items_center()
                            .justify_end()
                            .gap(px(8.))
                            .invisible()
                            .group_hover("user-message", |s| s.visible())
                            .text_size(px(12.))
                            .text_color(c.text_2)
                            .child(SharedString::from(clock_time(&message.created_at)))
                            .child(copy_button(format!("copy-{id}"), message.text.clone(), cx)),
                    )
                    .into_any_element()
            }
            OrchestrationMessageRole::Assistant => div()
                .group("assistant-message")
                .px(px(4.))
                .py(px(2.))
                .min_w_0()
                .child(if message.text.trim().is_empty() && !message.streaming {
                    div().text_size(px(14.)).text_color(c.text_2).child("(empty response)").into_any_element()
                } else {
                    markdown::render_styled(format!("a{id}"), &message.text, MdStyle::web(c.text.opacity(0.8)), cx)
                })
                .when(show_meta, |el| {
                    el.child(
                        div()
                            .mt(px(6.))
                            .invisible()
                            .group_hover("assistant-message", |s| s.visible())
                            .child(self.render_meta(message, cx)),
                    )
                })
                .when_some(diff.cloned().filter(|d| !d.files.is_empty()), |el, diff| {
                    el.child(div().mt(px(16.)).child(self.render_diff(index, &diff, cx)))
                })
                .into_any_element(),
            OrchestrationMessageRole::Reasoning => {
                let key = format!("m:{id}");
                let open = self.expanded.contains(&key);
                self.render_reasoning(index, &key, "Thought", &message.text, open, cx)
            }
            // The web draws nothing for them (only the row's space).
            OrchestrationMessageRole::System => div().into_any_element(),
        }
    }

    /// An answer's copy button and time (`AssistantTimelineRow`'s meta).
    fn render_meta(&self, message: &zc_contracts::OrchestrationMessage, cx: &App) -> AnyElement {
        let c = cx.theme().colors.clone();
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .text_size(px(12.))
            .line_height(px(16.))
            .text_color(c.text_2)
            .when(!message.text.trim().is_empty(), |el| {
                el.child(copy_button(format!("copy-{}", message.id.as_str()), message.text.clone(), cx))
            })
            .child(SharedString::from(clock_time(&message.updated_at)))
            .into_any_element()
    }

    /// A thinking block: "Thought" with a brain, opening into its text (`ReasoningTimelineRow`).
    fn render_reasoning(&mut self, index: usize, key: &str, label: &str, text: &str, open: bool, cx: &mut Context<Self>) -> AnyElement {
        let c = cx.theme().colors.clone();
        let key_owned = key.to_owned();
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .id(SharedString::from(format!("reason-{key}")))
                    .min_h(px(24.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .pl(px(2.))
                    .pr(px(8.))
                    .rounded(px(radius::MD))
                    .cursor_pointer()
                    .hover(|s| s.bg(c.accent_soft.opacity(0.2)))
                    .text_size(px(14.))
                    .line_height(px(22.75))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle(key_owned.clone(), index, cx)))
                    .child(icon_box(Icon::Brain, muted_icon(cx)))
                    .child(div().text_color(c.text_2).child(SharedString::from(label.to_owned())))
                    .child(chevron(open, cx)),
            )
            .when(open, |el| {
                el.child(
                    div()
                        .ml(px(28.))
                        .mt(px(4.))
                        .px(px(2.))
                        .py(px(4.))
                        .child(markdown::render_styled(format!("r{key}"), text, MdStyle::web(c.text), cx)),
                )
            })
            .into_any_element()
    }

    /// "Worked for 7m 7s ›" over its rule (`TurnFoldTimelineRow`).
    fn render_fold(&mut self, turn_id: &str, label: &str, expanded: bool, cx: &mut Context<Self>) -> AnyElement {
        let c = cx.theme().colors.clone();
        let turn = turn_id.to_owned();
        let text = c.text;
        div()
            .flex()
            .items_center()
            .gap(px(4.))
            .pt(px(4.))
            .pb(px(8.))
            .pr(px(2.))
            .border_b_1()
            .border_color(c.line.opacity(c.line.a * 0.6))
            .child(
                div()
                    .id(SharedString::from(format!("fold-{turn_id}")))
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .px(px(4.))
                    .rounded(px(radius::MD))
                    .cursor_pointer()
                    .text_size(px(14.))
                    .line_height(px(22.75))
                    .text_color(c.text_2)
                    .hover(move |s| s.text_color(text))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_turn(turn.clone(), cx)))
                    .child(SharedString::from(label.to_owned()))
                    .child(
                        svg()
                            .path(if expanded { Icon::ChevronDown.path() } else { Icon::ChevronRight.path() })
                            .size(px(14.))
                            .text_color(c.text_2),
                    ),
            )
            .into_any_element()
    }

    /// "Ran 3 commands", "Received 2 updates" (`WorkGroupToggleTimelineRow`).
    fn render_toggle(&mut self, group_id: &str, summary: &str, kind: SummaryKind, expanded: bool, failed: bool, cx: &mut Context<Self>) -> AnyElement {
        let c = cx.theme().colors.clone();
        let group = group_id.to_owned();
        div()
            .id(SharedString::from(format!("toggle-{group_id}")))
            .w_full()
            .min_h(px(24.))
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(2.))
            .py(px(2.))
            .rounded(px(radius::MD))
            .cursor_pointer()
            .hover(|s| s.bg(c.accent_soft.opacity(0.2)))
            .text_size(px(14.))
            .line_height(px(22.75))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_group(group.clone(), cx)))
            .child(icon_box(summary_icon(kind), if failed { c.danger.opacity(0.4) } else { muted_icon(cx) }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .one_line()
                    .text_color(c.text_2)
                    .child(SharedString::from(summary.to_owned())),
            )
            .when(expanded, |el| el.bg(c.accent_soft.opacity(0.2)))
            .into_any_element()
    }

    /// One work entry, or the entries of an open group (`PlainWorkEntryRow`).
    fn render_work_entries(
        &mut self,
        index: usize,
        id: &str,
        entries: &[WorkEntry],
        expanded_group: bool,
        label: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let root = self.workspace_root(cx);
        let rows: Vec<AnyElement> = entries
            .iter()
            .map(|entry| {
                let text = match (label, entries.len()) {
                    (Some(label), 1) => label.to_owned(),
                    _ => entry.display_label(root.as_deref()),
                };
                self.render_entry_row(index, &format!("{id}/{}", entry.id), entry, text, expanded_group, cx)
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .when(expanded_group, |el| el.max_h(px(288.)))
            .children(rows)
            .into_any_element()
    }

    fn render_entry_row(&mut self, index: usize, key: &str, entry: &WorkEntry, label: String, in_group: bool, cx: &mut Context<Self>) -> AnyElement {
        let c = cx.theme().colors.clone();
        let tool = entry.is_tool_like();
        let severe = entry.kind == "runtime.error" || entry.kind.ends_with(".failed");
        let warning = entry.kind == "runtime.warning";
        let failed = entry.display_failed();
        let (glyph, icon_color, label_color, weight) = if severe {
            (Icon::CircleAlert, c.danger, c.danger, FontWeight::MEDIUM)
        } else if warning {
            (Icon::CircleAlert, c.warning, c.warning, FontWeight::MEDIUM)
        } else if tool {
            (
                work_icon(entry),
                if failed { c.danger.opacity(0.4) } else { muted_icon(cx) },
                c.text_2,
                FontWeight::NORMAL,
            )
        } else {
            let glyph = match entry.tone {
                worklog::Tone::Info => Icon::Check,
                worklog::Tone::Thinking => Icon::Brain,
                _ => Icon::Zap,
            };
            (glyph, muted_icon(cx), c.text.opacity(0.8), FontWeight::NORMAL)
        };
        let body = expanded_body(entry, &label);
        let expandable = body.is_some();
        let open = self.expanded.contains(key);
        let key_owned = key.to_owned();
        div()
            .id(SharedString::from(format!("entry-{key}")))
            .flex()
            .flex_col()
            .px(px(2.))
            .when(!in_group, |el| el.py(px(2.)))
            .when(in_group && open, |el| el.mb(px(4.)))
            .rounded(px(radius::MD))
            .when(expandable, |el| {
                el.cursor_pointer()
                    .hover(|s| s.bg(c.accent_soft.opacity(0.2)))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle(key_owned.clone(), index, cx)))
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(icon_box(glyph, icon_color))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .when(!open, |el| el.one_line())
                            .text_size(px(14.))
                            .line_height(px(22.75))
                            .font_weight(weight)
                            .text_color(label_color)
                            // Closed, the label is one line, its breaks read as spaces (CSS).
                            .child(SharedString::from(if open {
                                label
                            } else {
                                label.split_whitespace().collect::<Vec<_>>().join(" ")
                            })),
                    )
                    .child(
                        div()
                            .size(px(16.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .when(!expandable, |el| el.invisible())
                            .child(
                                svg()
                                    .path(if open { Icon::ChevronDown.path() } else { Icon::ChevronRight.path() })
                                    .size(px(12.))
                                    .text_color(c.text_3.opacity(0.7)),
                            ),
                    ),
            )
            .when_some(body.filter(|_| open), |el, body| {
                el.child(
                    div()
                        .mt(px(4.))
                        .ml(px(28.))
                        .rounded(px(radius::MD))
                        .bg(c.muted.opacity(c.muted.a * 0.4))
                        .px(px(12.))
                        .py(px(8.))
                        .max_h(px(256.))
                        .overflow_hidden()
                        .font_family(MONO_FONT)
                        .text_size(px(13.))
                        .line_height(px(21.125))
                        .text_color(c.text_2)
                        .child(SharedString::from(body)),
                )
            })
            .into_any_element()
    }

    /// Thinking with tool calls (`ActivityGroupTimelineRow`).
    fn render_activity(&mut self, index: usize, group_id: &str, entries: &[RowEntry], expanded: bool, active: bool, cx: &mut Context<Self>) -> AnyElement {
        let c = cx.theme().colors.clone();
        let work: Vec<&WorkEntry> = entries
            .iter()
            .filter_map(|e| match e {
                RowEntry::Work(w) => Some(w),
                _ => None,
            })
            .collect();
        let thoughts = entries.iter().filter(|e| matches!(e, RowEntry::Message(_))).count();
        let (glyph, label) = if active {
            match work.iter().rev().find(|w| w.status == Some(worklog::ToolStatus::InProgress)) {
                Some(w) => (work_icon(w), w.live_label(self.workspace_root(cx).as_deref(), true)),
                None => (Icon::Brain, "Thinking".to_owned()),
            }
        } else if !work.is_empty() {
            (summary_icon(rows::summary_kind(&work)), worklog::summarize(&work))
        } else if thoughts > 1 {
            (Icon::Brain, format!("Thought (×{thoughts})"))
        } else {
            (Icon::Brain, "Thought".to_owned())
        };
        let group = group_id.to_owned();
        let mut children: Vec<AnyElement> = Vec::new();
        if expanded {
            let root = self.workspace_root(cx);
            for entry in entries {
                match entry {
                    RowEntry::Work(w) => {
                        let text = w.display_label(root.as_deref());
                        children.push(self.render_entry_row(index, &format!("{group_id}/{}", w.id), w, text, true, cx));
                    }
                    RowEntry::Message(m) => {
                        let key = format!("m:{}", m.id.as_str());
                        let open = self.expanded.contains(&key);
                        let preview = m.text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("Thought").to_owned();
                        children.push(self.render_reasoning(index, &key, &preview, &m.text, open, cx));
                    }
                    RowEntry::Plan(_) => {}
                }
            }
        }
        div()
            .flex()
            .flex_col()
            .child(
                live_row(glyph, label.into(), cx)
                    .id(SharedString::from(format!("activity-{group_id}")))
                    .cursor_pointer()
                    .hover(|s| s.bg(c.accent_soft.opacity(0.2)))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_group(group.clone(), cx))),
            )
            .when(expanded, |el| el.child(div().mt(px(8.)).flex().flex_col().children(children)))
            .into_any_element()
    }

    /// "Working for 2m 3s" over its rule (`WorkingTimelineRow`).
    fn render_working(&self, since: Option<&str>, cx: &App) -> AnyElement {
        let c = cx.theme().colors.clone();
        let label = match since.and_then(millis) {
            Some(since) => {
                let ms = (now_millis() - since).max(0);
                format!("Working for {}", if ms < 60_000 { format!("{}s", ms / 1000) } else { format_duration(ms) })
            }
            None => "Working...".to_owned(),
        };
        div()
            .pt(px(4.))
            .pb(px(8.))
            .border_b_1()
            .border_color(c.line.opacity(c.line.a * 0.6))
            .child(
                div()
                    .h(px(24.))
                    .px(px(4.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_size(px(14.))
                    .line_height(px(22.75))
                    .text_color(c.text_2)
                    .child(SharedString::from(label)),
            )
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
                                .child(div().flex_1().one_line().text_color(c.text_2).child(SharedString::from(f.path.clone())))
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
    /// The session's error, under the timeline (the web shows "Working for…" in the timeline).
    fn render_status_line(&self, cx: &App) -> Option<AnyElement> {
        let c = cx.theme().colors.clone();
        let thread = self.thread(cx)?;
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
                .child(div().one_line().child(SharedString::from(error)))
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
        self.render_page(cx)
    }
}

impl ThreadView {
    fn render_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
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

        // The new thread page (`DraftHeroHeadline`): the composer in the middle, the question
        // 32 px over it.
        if draft && self.pending_send.is_none() {
            let project = project_title.unwrap_or_default();
            let text = c.text;
            let heading = div()
                .absolute()
                .left_0()
                .right_0()
                .bottom(relative(1.))
                .pb(px(32.))
                .flex()
                .justify_center()
                .text_size(px(text::XXL))
                .line_height(px(36.))
                .text_color(c.text)
                .child("What should we build in\u{a0}")
                .child(
                    div()
                        .id("draft-project")
                        .font_weight(FontWeight::MEDIUM)
                        .cursor_pointer()
                        .border_b_1()
                        .border_dashed()
                        .border_color(c.text.opacity(0.3))
                        .hover(move |s| s.border_color(text))
                        .child(SharedString::from(project)),
                )
                .child("?");
            return div()
                .size_full()
                .flex()
                .flex_col()
                .justify_center()
                .px(px(20.))
                .child(
                    div()
                        .relative()
                        .w_full()
                        .max_w(px(COLUMN))
                        .mx_auto()
                        .child(heading)
                        .when_some(requests, |el, requests| el.child(requests))
                        .child(self.composer.clone())
                        .child(div().h(px(20.))),
                )
                .when_some(self.menu.as_ref(), |el, menu| el.child(menu.render()))
                .into_any_element();
        }
        let body: AnyElement = if !loaded && !draft {
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
                        let width = (bounds.size.width - px(40.)).max(px(200.));
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
                // The web's composer stack: 8 px over it, 20 under it, 20 px gutters.
                div().flex().flex_col().px(px(20.)).pt(px(8.)).pb(px(20.)).child(
                    div()
                        .map(|this| match self.width {
                            Some(width) => this.w(width.min(px(COLUMN))),
                            None => this.w_full().max_w(px(COLUMN)),
                        })
                        .mx_auto()
                        .flex()
                        .flex_col()
                        .when_some(requests, |this, requests| {
                            this.child(div().id("thread-requests").min_w_0().max_h(px(420.)).overflow_y_scroll().child(requests))
                        })
                        .when_some(status_line, |this, line| this.child(line))
                        .child(self.composer.clone()),
                ),
            )
            .when_some(terminals, |this, terminals| this.child(terminals))
            .when_some(self.menu.as_ref(), |this, menu| this.child(menu.render()))
            .into_any_element()
    }
}

/// `color` over `under`, as one opaque color.
fn over(color: Hsla, under: Hsla) -> Hsla {
    let (a, b) = (color.to_rgb(), under.to_rgb());
    let alpha = color.a;
    gpui::Rgba {
        r: a.r * alpha + b.r * (1. - alpha),
        g: a.g * alpha + b.g * (1. - alpha),
        b: a.b * alpha + b.b * (1. - alpha),
        a: 1.,
    }
    .into()
}

/// The work log's muted icons (`text-icon-muted opacity-70 light:brightness-60`), as one color.
fn muted_icon(cx: &App) -> Hsla {
    let theme = cx.theme();
    let c = &theme.colors;
    let base = c.text_3.to_rgb();
    let dim = if theme.mode == crate::theme::Mode::Light { 0.6 } else { 1. };
    over(
        gpui::Rgba {
            r: base.r * dim,
            g: base.g * dim,
            b: base.b * dim,
            a: 0.7,
        }
        .into(),
        c.bg_raised,
    )
}

/// A 16 px icon in its 24 px box.
fn icon_box(glyph: Icon, color: Hsla) -> gpui::Div {
    div()
        .size(px(24.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .child(svg().path(glyph.path()).size(px(16.)).text_color(color))
}

fn chevron(open: bool, cx: &App) -> gpui::Svg {
    svg()
        .path(if open { Icon::ChevronDown.path() } else { Icon::ChevronRight.path() })
        .size(px(12.))
        .flex_none()
        .text_color(cx.theme().colors.text_3.opacity(0.7))
}

/// A live or settled activity line (`LiveActivityRow`): its icon and label in muted text.
fn live_row(glyph: Icon, label: SharedString, cx: &App) -> gpui::Div {
    let c = cx.theme().colors.clone();
    div()
        .min_h(px(24.))
        .max_w_full()
        .flex()
        .items_center()
        .gap(px(6.))
        .px(px(2.))
        .py(px(2.))
        .rounded(px(radius::MD))
        .text_size(px(14.))
        .line_height(px(22.75))
        .text_color(c.text_2)
        .child(icon_box(glyph, muted_icon(cx)))
        .child(
            div()
                .min_w_0()
                .one_line()
                .child(SharedString::from(label.split_whitespace().collect::<Vec<_>>().join(" "))),
        )
}

/// A work entry's icon (`workEntryIconName`).
fn work_icon(entry: &WorkEntry) -> Icon {
    match entry.action() {
        worklog::Action::Read => Icon::Eye,
        worklog::Action::Edit => Icon::NewThread,
        worklog::Action::Command => Icon::Terminal,
        worklog::Action::WebSearch => Icon::Globe,
        worklog::Action::CodeSearch => Icon::Search,
        _ => match entry.item_type.as_deref() {
            Some("mcp_tool_call") => Icon::Wrench,
            Some("dynamic_tool_call") => Icon::Hammer,
            Some("collab_agent_tool_call") => Icon::Bot,
            _ => match entry.tone {
                worklog::Tone::Error => Icon::CircleAlert,
                worklog::Tone::Thinking => Icon::Brain,
                worklog::Tone::Info => Icon::Check,
                worklog::Tone::Tool => Icon::Zap,
            },
        },
    }
}

/// A group's icon (`toolGroupSummaryIconName`).
fn summary_icon(kind: SummaryKind) -> Icon {
    match kind {
        SummaryKind::Action(worklog::Action::Read) => Icon::Eye,
        SummaryKind::Action(worklog::Action::Edit) => Icon::NewThread,
        SummaryKind::Action(worklog::Action::Command) => Icon::Terminal,
        SummaryKind::Action(worklog::Action::WebSearch) => Icon::Globe,
        SummaryKind::Action(worklog::Action::CodeSearch) => Icon::Search,
        SummaryKind::Action(worklog::Action::Other) => Icon::Wrench,
        SummaryKind::Action(worklog::Action::Update) | SummaryKind::Mixed | SummaryKind::DynamicTool => Icon::Hammer,
        SummaryKind::AgentTool => Icon::Bot,
        SummaryKind::ToneTool => Icon::Zap,
    }
}

/// What an entry opens into (`buildToolCallExpandedBody`): the command, the detail, then the
/// changed files, without repeating the label.
fn expanded_body(entry: &WorkEntry, label: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for part in [entry.command.clone(), entry.detail.clone()].into_iter().flatten() {
        let part = part.trim().to_owned();
        if !part.is_empty() && part != label.trim() && !parts.contains(&part) {
            parts.push(part);
        }
    }
    if !entry.changed_files.is_empty() {
        let files = entry.changed_files.join("\n");
        if files != label.trim() {
            parts.push(files);
        }
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// A context compaction: its label between two lines.
fn render_compaction(label: &str, cx: &App) -> AnyElement {
    let c = cx.theme().colors.clone();
    let line = c.line.opacity(c.line.a * 0.7);
    div()
        .flex()
        .items_center()
        .gap(px(12.))
        .py(px(4.))
        .text_size(px(12.))
        .text_color(c.text_2)
        .child(div().h(px(1.)).flex_1().bg(line))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(4.))
                .child(svg().path(Icon::Minimize2.path()).size(px(12.)).text_color(c.text_2))
                .child(SharedString::from(label.to_owned())),
        )
        .child(div().h(px(1.)).flex_1().bg(line))
        .into_any_element()
}

/// An `xs` ghost-muted button with a label ("Show full message").
fn ghost_xs(id: SharedString, label: &'static str, cx: &App) -> gpui::Stateful<gpui::Div> {
    let c = cx.theme().colors.clone();
    let (text, accent) = (c.text, c.accent_soft);
    div()
        .id(id)
        .h(px(24.))
        .px(px(7.))
        .flex()
        .items_center()
        .rounded(px(radius::MD))
        .cursor_pointer()
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(c.text_2)
        .hover(move |s| s.bg(accent).text_color(text))
        .child(label)
}

/// The copy button under a message (`MessageCopyButton`).
fn copy_button(id: String, text: String, cx: &App) -> gpui::Stateful<gpui::Div> {
    let c = cx.theme().colors.clone();
    let accent = c.accent_soft;
    div()
        .id(SharedString::from(id))
        .size(px(24.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(radius::MD))
        .cursor_pointer()
        .hover(move |s| s.bg(accent))
        .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(text.clone())))
        .child(svg().path(Icon::Copy.path()).size(px(12.)).text_color(c.text_2))
}

/// "16:07": a message's local time of day.
fn clock_time(iso: &str) -> String {
    let Some(ms) = millis(iso) else { return String::new() };
    let Ok(ts) = jiff::Timestamp::from_millisecond(ms) else {
        return String::new();
    };
    let zoned = ts.to_zoned(jiff::tz::TimeZone::system());
    format!("{:02}:{:02}", zoned.hour(), zoned.minute())
}

/// A project script's icon (`projectScriptEditor.tsx`).
fn script_icon(icon: zc_contracts::ProjectScriptIcon) -> Icon {
    use zc_contracts::ProjectScriptIcon as I;
    match icon {
        I::Play => Icon::Play,
        I::Test => Icon::FlaskConical,
        I::Lint => Icon::ListChecks,
        I::Configure => Icon::Wrench,
        I::Build => Icon::Hammer,
        I::Debug => Icon::Bug,
    }
}

/// The git action's icon (`GitQuickActionIcon`): the host's mark for pull requests.
fn quick_icon(quick: &git::QuickAction) -> Icon {
    match &quick.kind {
        git::QuickKind::OpenPr => Icon::GitHub,
        git::QuickKind::Publish => Icon::CloudUpload,
        git::QuickKind::Pull => Icon::CloudDownload,
        git::QuickKind::Run("commit") => Icon::GitCommit,
        git::QuickKind::Run("push" | "commit_push") => Icon::CloudUpload,
        git::QuickKind::Run(_) => Icon::GitHub,
        git::QuickKind::Hint(_) if quick.label == "Commit" => Icon::GitCommit,
        git::QuickKind::Hint(_) if quick.label == "Push" => Icon::CloudUpload,
        git::QuickKind::Hint(_) => Icon::Info,
    }
}

/// How the web names an editor (`OpenInPicker.tsx`).
fn editor_name(id: &str) -> String {
    match id {
        "cursor" => "Cursor",
        "trae" => "Trae",
        "kiro" => "Kiro",
        "vscode" => "VS Code",
        "vscode-insiders" => "VS Code Insiders",
        "vscodium" => "VSCodium",
        "zed" => "Zed",
        "antigravity" => "Antigravity",
        "idea" => "IntelliJ IDEA",
        "file-manager" => {
            if cfg!(target_os = "macos") {
                "Finder"
            } else {
                "File manager"
            }
        }
        other => other,
    }
    .to_owned()
}
