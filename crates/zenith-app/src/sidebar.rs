//! The sidebar (glass, beside the work): search, a project filter, and every thread in four
//! sections (Pinned, Active, Snoozed, Settled; see `zenith_model::shell`), each row with its
//! status, project and age, and a menu of what can be done with it.

use std::collections::HashSet;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    div, px, App, ClipboardItem, Context, Entity, EventEmitter, FontWeight, Hsla, MouseButton, MouseDownEvent, Pixels, Point, PromptLevel, SharedString,
    Subscription, Task, Window,
};
use serde_json::json;
use zc_contracts::{OrchestrationThreadShell, ProjectId, ThreadId};
use zenith_model::shell::{self, Section, ThreadStatus};
use zenith_model::time::{duration, now_millis, short_age};

use crate::assets::Icon;
use crate::store::{self, Store};
use crate::theme::{radius, text, ActiveTheme};
use crate::ui::menu::{Entry, OpenMenu};
use crate::ui::text_area::{TextArea, TextAreaEvent};
use crate::ui::{caps_label, dot, icon, spinner, Button, Tooltip};
use crate::workspace::{title_bar, TRAFFIC_LIGHTS};

pub enum SidebarEvent {
    OpenThread(ThreadId),
    NewThread(Option<ProjectId>),
    AddProject,
    OpenSettings,
    OpenSessions,
    ToggleSidebar,
    /// A thread was archived or deleted from the sidebar.
    Removed(ThreadId),
}

pub struct Sidebar {
    store: Entity<Store>,
    search: Entity<TextArea>,
    project: Option<ProjectId>,
    selected: Option<ThreadId>,
    collapsed: HashSet<Section>,
    renaming: Option<(ThreadId, Entity<TextArea>, Subscription)>,
    menu: Option<OpenMenu>,
    _tick: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SidebarEvent> for Sidebar {}

/// Colors and labels of a thread's status.
pub fn status_color(status: ThreadStatus, cx: &App) -> Hsla {
    let c = &cx.theme().colors;
    match status {
        ThreadStatus::Approval => c.warning,
        ThreadStatus::Input => c.accent,
        ThreadStatus::Working | ThreadStatus::Monitoring => c.accent,
        ThreadStatus::Failed => c.danger,
        ThreadStatus::PlanReady => c.accent_hover,
        ThreadStatus::Ready => c.text_3,
    }
}

impl Sidebar {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = store::store(cx);
        let search = cx.new(|cx| TextArea::single_line(cx).with_placeholder("Search threads"));
        let subscriptions = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.subscribe_in(&search, window, |this, _, event: &TextAreaEvent, window, cx| match event {
                TextAreaEvent::Changed => cx.notify(),
                TextAreaEvent::Cancel => {
                    this.search.update(cx, |s, cx| s.clear(cx));
                    window.blur();
                }
                TextAreaEvent::Submit | TextAreaEvent::MoveDown => {
                    if let Some(first) = this.first(cx) {
                        cx.emit(SidebarEvent::OpenThread(first));
                    }
                }
                TextAreaEvent::MoveUp | TextAreaEvent::PastedImage { .. } => {}
            }),
        ];
        // Ages and "Working 2m" tick.
        let tick = cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(15)).await;
            if this.update(cx, |_, cx| cx.notify()).is_err() {
                return;
            }
        });
        let mut collapsed = HashSet::new();
        collapsed.insert(Section::Settled);
        Self {
            store,
            search,
            project: None,
            selected: None,
            collapsed,
            renaming: None,
            menu: None,
            _tick: tick,
            _subscriptions: subscriptions,
        }
    }

    pub fn set_selected(&mut self, id: Option<ThreadId>, cx: &mut Context<Self>) {
        self.selected = id;
        cx.notify();
    }

    pub fn project_filter(&self) -> Option<ProjectId> {
        self.project.clone()
    }

    fn visible(&self, cx: &App) -> Vec<ThreadId> {
        let store = self.store.read(cx);
        let query = self.search.read(cx).text().to_owned();
        store
            .shell
            .sidebar(self.project.as_ref(), &query, now_millis())
            .into_iter()
            .filter(|(section, _)| !self.collapsed.contains(section) || !query.is_empty())
            .flat_map(|(_, threads)| threads.into_iter().map(|t| t.id.clone()).collect::<Vec<_>>())
            .collect()
    }

    pub fn first(&self, cx: &App) -> Option<ThreadId> {
        self.visible(cx).into_iter().next()
    }

    /// The thread `delta` rows away from `id` in the sidebar's order.
    pub fn neighbor(&self, id: &ThreadId, delta: isize, cx: &App) -> Option<ThreadId> {
        let visible = self.visible(cx);
        let index = visible.iter().position(|t| t == id)?;
        let next = index as isize + delta;
        (next >= 0).then(|| visible.get(next as usize).cloned()).flatten()
    }

    /// Runs a registry command (`zenith_commands`), as the CLI and agents would.
    fn command(&self, name: &'static str, params: serde_json::Value, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| s.run_command(name, params, cx)).detach();
    }

    fn open_project_menu(&mut self, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let store = self.store.read(cx);
        let mut entries = vec![{
            let this = this.clone();
            Entry::item("All projects", move |_, cx| {
                let _ = this.update(cx, |s, cx| {
                    s.project = None;
                    cx.notify();
                });
            })
            .checked(self.project.is_none())
        }];
        entries.push(Entry::Separator);
        for project in store.shell.projects_sorted() {
            let id = project.id.clone();
            let this = this.clone();
            entries.push(
                Entry::item(project.title.clone(), move |_, cx| {
                    let _ = this.update(cx, |s, cx| {
                        s.project = Some(id.clone());
                        cx.notify();
                    });
                })
                .icon(Icon::Folder)
                .checked(self.project.as_ref() == Some(&project.id)),
            );
        }
        entries.push(Entry::Separator);
        let add = this.clone();
        entries.push(
            Entry::item("Add project…", move |_, cx| {
                let _ = add.update(cx, |_, cx| cx.emit(SidebarEvent::AddProject));
            })
            .icon(Icon::FolderPlus)
            .keys("⌘O"),
        );
        self.menu = Some(OpenMenu::new(entries, position, window, cx, |this, _, _| this.menu = None));
        cx.notify();
    }

    fn open_thread_menu(&mut self, thread: &OrchestrationThreadShell, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let id = thread.id.clone();
        let now = now_millis();
        let section = shell::section(thread, now);
        let mut entries = Vec::new();
        let tid = id.as_str().to_owned();
        let cmd = |this: &gpui::WeakEntity<Self>, name: &'static str, params: serde_json::Value| {
            let this = this.clone();
            move |_: &mut Window, cx: &mut App| {
                let _ = this.update(cx, |s, cx| s.command(name, params.clone(), cx));
            }
        };
        let on_thread = json!({"threadId": tid});
        entries.push(
            Entry::item(
                if section == Section::Pinned { "Unpin" } else { "Pin" },
                cmd(&this, if section == Section::Pinned { "thread.unpin" } else { "thread.pin" }, on_thread.clone()),
            )
            .icon(if section == Section::Pinned { Icon::PinOff } else { Icon::Pin }),
        );
        entries.push(
            Entry::item(
                if section == Section::Settled { "Reopen" } else { "Settle" },
                cmd(
                    &this,
                    if section == Section::Settled { "thread.reopen" } else { "thread.settle" },
                    on_thread.clone(),
                ),
            )
            .icon(Icon::CircleCheck)
            .keys("⌘⇧S"),
        );
        if section == Section::Snoozed {
            entries.push(Entry::item("Wake now", cmd(&this, "thread.wake", on_thread.clone())).icon(Icon::Bell));
        } else if shell::can_snooze(thread) {
            entries.push(Entry::Header("Snooze".into()));
            for (label, until) in snooze_presets() {
                entries.push(Entry::item(label, cmd(&this, "thread.snooze", json!({"threadId": tid, "until": until}))).icon(Icon::Clock));
            }
        }
        entries.push(Entry::Separator);
        {
            let this = this.clone();
            let id = id.clone();
            let title = thread.title.clone();
            entries.push(
                Entry::item("Rename…", move |window, cx| {
                    let _ = this.update(cx, |s, cx| s.start_rename(id.clone(), &title, window, cx));
                })
                .icon(Icon::Pencil),
            );
        }
        entries.push(Entry::item("Regenerate title", cmd(&this, "thread.regenerateTitle", on_thread.clone())).icon(Icon::Sparkles));
        entries.push(Entry::Separator);
        let path = thread
            .worktree_path
            .clone()
            .or_else(|| self.store.read(cx).shell.project(&thread.project_id).map(|p| p.workspace_root.clone()));
        if let Some(path) = path {
            entries.push(Entry::item("Copy path", move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(path.clone()))).icon(Icon::Copy));
        }
        if let Some(branch) = thread.branch.clone() {
            entries.push(Entry::item("Copy branch", move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(branch.clone()))).icon(Icon::GitBranch));
        }
        {
            let tid = tid.clone();
            entries.push(Entry::item("Copy thread ID", move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(tid.clone()))).icon(Icon::Copy));
        }
        entries.push(Entry::Separator);
        let running = matches!(shell::status(thread), ThreadStatus::Working);
        {
            let this = this.clone();
            let id = id.clone();
            entries.push(
                Entry::item("Archive", move |_, cx| {
                    let _ = this.update(cx, |s, cx| {
                        s.command("thread.archive", json!({"threadId": id.as_str()}), cx);
                        cx.emit(SidebarEvent::Removed(id.clone()));
                    });
                })
                .icon(Icon::Archive)
                .disabled(running),
            );
        }
        {
            let this = this.clone();
            let id = id.clone();
            let title = thread.title.clone();
            entries.push(
                Entry::item("Delete…", move |window, cx| {
                    let answer = window.prompt(
                        PromptLevel::Warning,
                        &format!("Delete “{title}”?"),
                        Some("The thread and its messages are deleted. Its worktree, if any, stays on disk."),
                        &["Delete Thread", "Cancel"],
                        cx,
                    );
                    let this = this.clone();
                    let id = id.clone();
                    cx.spawn(async move |cx| {
                        if answer.await == Ok(0) {
                            let _ = this.update(cx, |s, cx| {
                                if running {
                                    s.command("thread.stop", json!({"threadId": id.as_str()}), cx);
                                }
                                s.command("thread.delete", json!({"threadId": id.as_str()}), cx);
                                cx.emit(SidebarEvent::Removed(id.clone()));
                            });
                        }
                    })
                    .detach();
                })
                .icon(Icon::Trash)
                .danger(),
            );
        }
        self.menu = Some(OpenMenu::new(entries, position, window, cx, |this, _, _| this.menu = None));
        cx.notify();
    }

    fn start_rename(&mut self, id: ThreadId, title: &str, window: &mut Window, cx: &mut Context<Self>) {
        let field = cx.new(|cx| {
            let mut field = TextArea::single_line(cx);
            field.set_text(title, cx);
            field
        });
        let renamed = id.clone();
        let subscription = cx.subscribe_in(&field, window, move |this, field, event: &TextAreaEvent, _, cx| match event {
            TextAreaEvent::Submit => {
                let title = field.read(cx).text().trim().to_owned();
                if !title.is_empty() {
                    this.command("thread.rename", json!({"threadId": renamed.as_str(), "title": title}), cx);
                }
                this.renaming = None;
                cx.notify();
            }
            TextAreaEvent::Cancel => {
                this.renaming = None;
                cx.notify();
            }
            _ => {}
        });
        field.update(cx, |f, _| f.focus(window));
        self.renaming = Some((id, field, subscription));
        cx.notify();
    }

    fn render_row(&self, thread: &OrchestrationThreadShell, show_project: bool, now: i64, cx: &mut Context<Self>) -> gpui::AnyElement {
        let c = cx.theme().colors.clone();
        let status = shell::status(thread);
        let selected = self.selected.as_ref() == Some(&thread.id);
        let store = self.store.read(cx);
        let project = store.shell.project(&thread.project_id).map(|p| p.title.clone());
        let id = thread.id.clone();
        let renaming = self.renaming.as_ref().filter(|(r, _, _)| r == &thread.id).map(|(_, field, _)| field.clone());
        let indicator = match status {
            ThreadStatus::Working => spinner(SharedString::from(format!("spin-{}", id.as_str())), c.accent, 12.).into_any_element(),
            ThreadStatus::Ready => dot(gpui::transparent_black()).into_any_element(),
            other => dot(status_color(other, cx)).into_any_element(),
        };
        let right: SharedString = match status {
            ThreadStatus::Working => shell::working_since(thread).map(|since| duration(now - since)).unwrap_or_default().into(),
            ThreadStatus::Ready => short_age(shell::activity_at(thread), now).into(),
            other => other.label().into(),
        };
        let subtitle: Option<SharedString> = match (show_project.then_some(project).flatten(), &thread.branch) {
            (Some(p), Some(b)) => Some(format!("{p} · {b}").into()),
            (Some(p), None) => Some(p.into()),
            (None, Some(b)) => Some(b.clone().into()),
            (None, None) => None,
        };
        let thread_for_menu = thread.clone();
        div()
            .id(SharedString::from(format!("row-{}", id.as_str())))
            .flex()
            .items_center()
            .gap(px(8.))
            .mx(px(8.))
            .px(px(8.))
            .py(px(6.))
            .rounded(px(radius::SM))
            .cursor_pointer()
            .when(selected, |this| this.bg(c.accent_soft))
            .when(!selected, |this| this.hover(|s| s.bg(c.hover)))
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(SidebarEvent::OpenThread(id.clone()))))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| this.open_thread_menu(&thread_for_menu, event.position, window, cx)),
            )
            .child(div().w(px(12.)).flex().justify_center().child(indicator))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(match renaming {
                        Some(field) => div().child(field).into_any_element(),
                        None => div()
                            .truncate()
                            .text_size(px(text::BASE))
                            .font_weight(if selected { FontWeight::SEMIBOLD } else { FontWeight::MEDIUM })
                            .text_color(c.text)
                            .child(thread.title.clone())
                            .into_any_element(),
                    })
                    .when_some(subtitle, |this, subtitle| {
                        this.child(div().truncate().text_size(px(text::XS)).text_color(c.text_3).child(subtitle))
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(text::XS))
                    .text_color(if status.needs_attention() { status_color(status, cx) } else { c.text_3 })
                    .when(status.needs_attention(), |this| this.font_weight(FontWeight::SEMIBOLD))
                    .child(right),
            )
            .into_any_element()
    }
}

/// Snooze choices: in an hour, in three, tomorrow 9:00, next Monday 9:00 (local time).
fn snooze_presets() -> Vec<(&'static str, String)> {
    let now = jiff::Zoned::now();
    let iso = |z: &jiff::Zoned| {
        zc_contracts::DateTimeUtc::from_millis(z.timestamp().as_millisecond())
            .map(|d| d.to_iso_string())
            .unwrap_or_default()
    };
    let mut presets = vec![
        ("For an hour", iso(&now.checked_add(jiff::Span::new().hours(1)).unwrap_or(now.clone()))),
        ("For three hours", iso(&now.checked_add(jiff::Span::new().hours(3)).unwrap_or(now.clone()))),
    ];
    let morning = |days: i64| {
        now.date()
            .checked_add(jiff::Span::new().days(days))
            .ok()
            .and_then(|d| d.at(9, 0, 0, 0).to_zoned(now.time_zone().clone()).ok())
    };
    if let Some(tomorrow) = morning(1) {
        presets.push(("Until tomorrow morning", iso(&tomorrow)));
    }
    let weekday = now.date().weekday().to_monday_one_offset() as i64;
    if let Some(monday) = morning(8 - weekday) {
        presets.push(("Until next week", iso(&monday)));
    }
    presets
}

impl Render for Sidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = cx.theme().colors.clone();
        let now = now_millis();
        let query = self.search.read(cx).text().to_owned();
        let store = self.store.read(cx);
        let sections: Vec<(Section, Vec<OrchestrationThreadShell>)> = store
            .shell
            .sidebar(self.project.as_ref(), &query, now)
            .into_iter()
            .map(|(section, threads)| (section, threads.into_iter().cloned().collect()))
            .collect();
        let project_title: SharedString = self
            .project
            .as_ref()
            .and_then(|p| store.shell.project(p))
            .map(|p| p.title.clone().into())
            .unwrap_or_else(|| "All projects".into());
        let connected = store.connected();
        let status: SharedString = match &store.status {
            zenith_client::ConnectionStatus::Connected => "Connected".into(),
            zenith_client::ConnectionStatus::Connecting => "Connecting…".into(),
            zenith_client::ConnectionStatus::Failed(reason) => SharedString::from(reason.clone()),
        };
        let shell_loaded = store.shell.loaded;
        let empty = sections.is_empty();
        let show_project = self.project.is_none();
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        for (section, threads) in &sections {
            let collapsed = self.collapsed.contains(section) && query.is_empty();
            let section_copy = *section;
            rows.push(
                div()
                    .id(SharedString::from(format!("section-{}", section.as_str())))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(16.))
                    .pt(px(14.))
                    .pb(px(4.))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.collapsed.remove(&section_copy) {
                            this.collapsed.insert(section_copy);
                        }
                        cx.notify();
                    }))
                    .child(caps_label(section.label(), cx))
                    .child(
                        div()
                            .text_size(px(text::XS))
                            .text_color(c.text_3)
                            .child(SharedString::from(threads.len().to_string())),
                    )
                    .child(div().flex_1())
                    .child(icon(if collapsed { Icon::ChevronRight } else { Icon::ChevronDown }, c.text_3).size(px(12.)))
                    .into_any_element(),
            );
            if !collapsed {
                for thread in threads {
                    rows.push(self.render_row(thread, show_project, now, cx));
                }
            }
        }

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                title_bar("sidebar-title-bar")
                    .pl(px(TRAFFIC_LIGHTS))
                    .pr(px(10.))
                    .gap(px(2.))
                    .child(div().flex_1())
                    .child(
                        Button::new("hide-sidebar")
                            .icon(Icon::PanelLeft)
                            .tooltip_keys("Hide the sidebar", "⌘B")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::ToggleSidebar))),
                    )
                    .child(
                        Button::new("new-thread")
                            .icon(Icon::NewThread)
                            .tooltip_keys("New thread", "⌘N")
                            .on_click(cx.listener(|this, _, _, cx| cx.emit(SidebarEvent::NewThread(this.project.clone())))),
                    ),
            )
            .child(
                div()
                    .px(px(12.))
                    .pb(px(6.))
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .h(px(30.))
                            .px(px(10.))
                            .rounded(px(radius::SM))
                            .bg(c.hover)
                            .child(icon(Icon::Search, c.text_3))
                            .child(div().flex_1().child(self.search.clone())),
                    )
                    .child(
                        div()
                            .id("project-filter")
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .h(px(26.))
                            .px(px(6.))
                            .rounded(px(radius::SM))
                            .cursor_pointer()
                            .hover(|s| s.bg(c.hover))
                            .text_size(px(text::SM))
                            .text_color(c.text_2)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, event: &MouseDownEvent, window, cx| this.open_project_menu(event.position, window, cx)),
                            )
                            .child(icon(Icon::Folder, c.text_3))
                            .child(div().flex_1().truncate().child(project_title))
                            .child(icon(Icon::ChevronDown, c.text_3).size(px(12.))),
                    ),
            )
            .child(
                div()
                    .id("threads")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .pb(px(12.))
                    .children(rows)
                    .when(empty && shell_loaded, |this| {
                        this.child(
                            div()
                                .px(px(20.))
                                .pt(px(24.))
                                .text_size(px(text::SM))
                                .text_color(c.text_3)
                                .child(if query.is_empty() { "No threads yet." } else { "No thread matches." }),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(2.))
                    .px(px(10.))
                    .h(px(44.))
                    .border_t_1()
                    .border_color(c.line)
                    .child(
                        Button::new("open-settings")
                            .icon(Icon::Settings)
                            .tooltip_keys("Settings", "⌘,")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::OpenSettings))),
                    )
                    .child(
                        Button::new("open-sessions")
                            .icon(Icon::Coins)
                            .tooltip_keys("Sessions & costs", "⌘U")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::OpenSessions))),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("connection")
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .px(px(6.))
                            .text_size(px(text::XS))
                            .text_color(c.text_3)
                            .tooltip(move |_, cx| Tooltip::view(status.clone(), None, cx))
                            .child(dot(if connected { c.success } else { c.warning }))
                            .child(if connected { "zenith" } else { "Offline" }),
                    ),
            )
            .when_some(self.menu.as_ref(), |this, menu| this.child(menu.render()))
    }
}
