//! The sidebar (glass, beside the work): search, a project filter, and every thread in four
//! sections (Pinned, Active, Snoozed, Settled; see `zenith_model::shell`), each row with its
//! status, project and age, and a menu of what can be done with it.

use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    div, px, svg, App, ClipboardItem, Context, Entity, EventEmitter, FontWeight, MouseButton, MouseDownEvent, Pixels, Point, PromptLevel, SharedString,
    Subscription, Task, Window,
};
use serde_json::json;
use zc_contracts::{OrchestrationThreadShell, ProjectId, ThreadId};
use zenith_model::shell::{self, Section, ThreadStatus};
use zenith_model::time::{now_millis, sidebar_age, working_label};

use crate::actions;
use crate::assets::Icon;
use crate::store::{self, Store};
use crate::theme::{radius, ActiveTheme};
use crate::ui::badges::{project_badge, provider_icon, pull_request_badge};
use crate::ui::menu::{Entry, OpenMenu};
use crate::ui::text_area::{TextArea, TextAreaEvent};
use crate::ui::{icon_button, OneLine, Tooltip};
use crate::workspace::title_bar;

pub enum SidebarEvent {
    OpenThread(ThreadId),
    NewThread(Option<ProjectId>),
    AddProject,
    OpenSettings,
    OpenSessions,
    OpenPullRequests,
    SettingsPage(crate::settings::SettingsPage),
    /// Leave the settings.
    Back,
    /// A thread was archived or deleted from the sidebar.
    Removed(ThreadId),
}

pub struct Sidebar {
    store: Entity<Store>,
    search: Entity<TextArea>,
    project: Option<ProjectId>,
    selected: Option<ThreadId>,
    collapsed: HashSet<Section>,
    /// The row under the pointer (its status gives way to its actions).
    hovered: Option<ThreadId>,
    /// On the settings, the sidebar lists their pages (`SettingsSidebarNav`).
    settings: Option<crate::settings::SettingsPage>,
    renaming: Option<(ThreadId, Entity<TextArea>, Subscription)>,
    menu: Option<OpenMenu>,
    _tick: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SidebarEvent> for Sidebar {}

impl Sidebar {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = store::store(cx);
        let search = cx.new(|cx| TextArea::single_line(cx).with_placeholder("Search").with_font(14., 21.));
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
        collapsed.insert(Section::Snoozed);
        Self {
            store,
            search,
            project: None,
            selected: None,
            collapsed,
            hovered: None,
            settings: None,
            renaming: None,
            menu: None,
            _tick: tick,
            _subscriptions: subscriptions,
        }
    }

    pub fn set_settings_page(&mut self, page: Option<crate::settings::SettingsPage>, cx: &mut Context<Self>) {
        self.settings = page;
        cx.notify();
    }

    /// The settings' pages, as the web lists them in its sidebar.
    fn render_settings_nav(&self, current: crate::settings::SettingsPage, cx: &mut Context<Self>) -> gpui::AnyElement {
        let c = cx.theme().colors.clone();
        let query = self.search.read(cx).text().trim().to_lowercase();
        let hover = c.hover;
        let item = |id: SharedString, glyph: Icon, label: &'static str, active: bool| {
            div()
                .id(id)
                .h(px(32.))
                .px(px(10.))
                .flex()
                .items_center()
                .gap(px(8.))
                .rounded(px(radius::MD))
                .cursor_pointer()
                .when(active, |el| el.bg(c.sidebar_row_selected))
                .when(!active, |el| el.hover(move |s| s.bg(hover)))
                .child(
                    svg()
                        .path(glyph.path())
                        .size(px(16.))
                        .flex_none()
                        .text_color(if active { c.text } else { c.sidebar_icon }),
                )
                .child(
                    div()
                        .text_size(px(14.))
                        .line_height(px(20.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(if active { c.text } else { c.text_2.opacity(0.8) })
                        .child(label),
                )
        };
        let pages = crate::settings::SettingsPage::ALL
            .into_iter()
            .filter(|p| query.is_empty() || p.label().to_lowercase().contains(&query))
            .map(|page| {
                item(
                    SharedString::from(format!("settings-{}", page.label())),
                    page.icon(),
                    page.label(),
                    page == current,
                )
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(SidebarEvent::SettingsPage(page))))
                .into_any_element()
            })
            .collect::<Vec<_>>();
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div().p(px(8.)).child(
                    div()
                        .h(px(32.))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(8.))
                        .rounded(px(radius::MD))
                        .hover(move |s| s.bg(hover))
                        .child(svg().path(Icon::Search.path()).size(px(16.)).flex_none().text_color(c.text_2.opacity(0.8)))
                        .child(div().flex_1().min_w_0().font_weight(FontWeight::MEDIUM).child(self.search.clone()))
                        .child(
                            div()
                                .size(px(20.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(4.))
                                .bg(c.muted)
                                .text_size(px(12.))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(c.text_2)
                                .child("/"),
                        ),
                ),
            )
            .child(div().px(px(8.)).flex().flex_col().gap(px(4.)).children(pages))
            .child(div().flex_1())
            .child(
                div()
                    .px(px(8.))
                    .py(px(4.))
                    .child(item("settings-back".into(), Icon::ArrowLeft, "Back", false).on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::Back)))),
            )
            .into_any_element()
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

    pub fn open_thread_menu(&mut self, thread: &OrchestrationThreadShell, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let rename = {
            let this = this.clone();
            let id = thread.id.clone();
            let title = thread.title.clone();
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let _ = this.update(cx, |s, cx| s.start_rename(id.clone(), &title, window, cx));
            }) as RenameThread
        };
        let removed = {
            let this = this.clone();
            let id = thread.id.clone();
            Rc::new(move |cx: &mut App| {
                let _ = this.update(cx, |_, cx| cx.emit(SidebarEvent::Removed(id.clone())));
            }) as ThreadRemoved
        };
        let entries = thread_menu_entries(thread, cx, rename, removed);
        self.menu = Some(OpenMenu::new(entries, position, window, cx, |this, _, _| this.menu = None));
        cx.notify();
    }

    pub fn start_rename(&mut self, id: ThreadId, title: &str, window: &mut Window, cx: &mut Context<Self>) {
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

    /// The web's thread card (`Sidebar.tsx`): 78 px, the project and the status on top, the
    /// title, then the worktree, branch, pull request and provider.
    fn render_row(&self, thread: &OrchestrationThreadShell, now: i64, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let c = theme.colors.clone();
        let status = shell::status(thread);
        let active = self.selected.as_ref() == Some(&thread.id);
        let hovered = self.hovered.as_ref() == Some(&thread.id);
        let store = self.store.read(cx);
        let project = store.shell.project(&thread.project_id).map(|p| p.title.clone()).unwrap_or_default();
        let id = thread.id.clone();
        let renaming = self.renaming.as_ref().filter(|(r, _, _)| r == &thread.id).map(|(_, field, _)| field.clone());
        // Rows recede when nothing asks for the person (`isSidebarRowReceding`).
        let receding = !active
            && matches!(
                status,
                ThreadStatus::Working | ThreadStatus::Monitoring | ThreadStatus::Ready | ThreadStatus::Approval | ThreadStatus::PlanReady
            );
        let fades = receding && matches!(status, ThreadStatus::Working | ThreadStatus::Monitoring);
        let title_color = if receding {
            c.text_2
        } else if status == ThreadStatus::Input {
            c.text
        } else if status == ThreadStatus::Failed {
            c.text.opacity(0.95)
        } else {
            c.text.opacity(0.9)
        };
        let weight = if receding { FontWeight::NORMAL } else { FontWeight::MEDIUM };
        let row_id: SharedString = format!("row-{}", id.as_str()).into();

        // The status at rest; the actions when hovered or open.
        let show_actions = hovered;
        let status_slot: gpui::AnyElement = if show_actions {
            let snooze = shell::can_snooze(thread).then(|| {
                let thread_for_menu = thread.clone();
                div()
                    .id(SharedString::from(format!("snooze-{}", id.as_str())))
                    .h_full()
                    .flex()
                    .items_center()
                    .px(px(6.))
                    .rounded(px(radius::MD))
                    .cursor_pointer()
                    .text_color(c.text_2)
                    .hover(|s| s.text_color(c.text))
                    .child(svg().path(Icon::Clock.path()).size(px(12.)).text_color(c.text_2))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.open_snooze_menu(&thread_for_menu, event.position, window, cx);
                        }),
                    )
            });
            let settled = shell::section(thread, now) == Section::Settled;
            let settle_id = id.clone();
            div()
                .flex()
                .items_center()
                .h_full()
                .children(snooze)
                .child(
                    div()
                        .id(SharedString::from(format!("settle-{}", id.as_str())))
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .px(px(6.))
                        .mr(px(-4.))
                        .rounded(px(radius::MD))
                        .cursor_pointer()
                        .text_size(px(12.))
                        .line_height(px(16.))
                        .text_color(c.text_2)
                        .hover(|s| s.text_color(c.text))
                        .child(svg().path(Icon::Check.path()).size(px(14.)).text_color(c.text_2))
                        .child(if settled { "Reopen" } else { "Settle" })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            let name = if settled { "thread.reopen" } else { "thread.settle" };
                            this.command(name, json!({"threadId": settle_id.as_str()}), cx);
                        })),
                )
                .into_any_element()
        } else {
            match status_label(thread, status, now, cx) {
                Some(label) => label.into_any_element(),
                None => div()
                    .text_size(px(12.))
                    .line_height(px(16.))
                    .text_color(c.text_2)
                    // From the last message sent, else the last change (`threadTimeLabel`).
                    .child(SharedString::from(sidebar_age(
                        thread
                            .latest_user_message_at
                            .as_deref()
                            .or(Some(thread.updated_at.as_str()))
                            .and_then(zenith_model::time::millis)
                            .unwrap_or(now),
                        now,
                    )))
                    .into_any_element(),
            }
        };

        let badge = shell::pull_request_badge(thread);
        let provider = thread.model_selection.instance_id.to_string();
        let thread_for_menu = thread.clone();
        let hover_id = id.clone();
        div()
            .py(px(2.))
            .child(
                div()
                    .id(row_id)
                    .relative()
                    .h(px(78.))
                    .px(px(10.))
                    .py(px(8.))
                    .rounded(px(radius::MD))
                    .overflow_hidden()
                    .cursor_pointer()
                    .when(active, |this| this.bg(c.sidebar_row_active))
                    .when(!active, |this| this.hover(|s| s.bg(c.hover)))
                    .when(fades && !hovered, |this| this.opacity(0.7))
                    .on_hover(cx.listener(move |this, hovering: &bool, _, cx| {
                        let next = hovering.then(|| hover_id.clone());
                        if *hovering || this.hovered.as_ref() == Some(&hover_id) {
                            this.hovered = next;
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(SidebarEvent::OpenThread(id.clone()))))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| this.open_thread_menu(&thread_for_menu, event.position, window, cx)),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .h(px(20.))
                            .gap(px(6.))
                            .child(project_badge(&project, 16., cx))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .one_line()
                                    .text_size(px(12.))
                                    .line_height(px(16.))
                                    .font_weight(weight)
                                    .text_color(c.text_2)
                                    .child(SharedString::from(project.clone())),
                            )
                            .when(shell::section(thread, now) == Section::Pinned, |this| {
                                this.child(svg().path(Icon::Pin.path()).size(px(12.)).flex_none().text_color(c.text_2.opacity(0.65)))
                            })
                            .child(
                                div()
                                    .h(px(20.))
                                    .min_w(px(32.))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_end()
                                    .child(status_slot),
                            ),
                    )
                    .child(
                        // A block at full width: GPUI only adds the ellipsis when the width is
                        // known as the text is measured.
                        div().mt(px(4.)).w_full().child(match renaming {
                            Some(field) => div().w_full().child(field).into_any_element(),
                            None => div()
                                .w_full()
                                .one_line()
                                .text_size(px(14.))
                                .line_height(px(20.))
                                .font_weight(weight)
                                .text_color(title_color)
                                .child(thread.title.clone())
                                .into_any_element(),
                        }),
                    )
                    .child(
                        div()
                            .mt(px(2.))
                            .h(px(16.))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .min_w_0()
                            .text_size(px(12.))
                            .line_height(px(16.))
                            .text_color(c.text_2)
                            .when(thread.worktree_path.is_some(), |this| {
                                this.child(svg().path(Icon::FolderGit2.path()).size(px(12.)).flex_none().text_color(c.text_2.opacity(0.4)))
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .one_line()
                                    .text_color(c.text_2.opacity(0.4))
                                    .children(thread.branch.clone().map(|b| SharedString::from(middle_truncate(&b)))),
                            )
                            .children(badge.as_ref().map(|badge| pull_request_badge(badge, cx)))
                            .child(
                                div()
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .gap(px(4.))
                                    .child(provider_icon(&provider, 14., cx).opacity(0.6)),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// A settled or snoozed thread: one 36 px line, dimmed until hovered.
    fn render_slim_row(&self, thread: &OrchestrationThreadShell, now: i64, cx: &mut Context<Self>) -> gpui::AnyElement {
        let c = cx.theme().colors.clone();
        let active = self.selected.as_ref() == Some(&thread.id);
        let hovered = self.hovered.as_ref() == Some(&thread.id);
        let store = self.store.read(cx);
        let project = store.shell.project(&thread.project_id).map(|p| p.title.clone()).unwrap_or_default();
        let id = thread.id.clone();
        let section = shell::section(thread, now);
        let lit = active || hovered;
        let dim = c.text_2.opacity(0.7);
        let time: SharedString = match section {
            Section::Settled => sidebar_age(
                thread
                    .settled_at
                    .as_deref()
                    .and_then(zenith_model::time::millis)
                    .unwrap_or_else(|| shell::activity_at(thread)),
                now,
            )
            .into(),
            _ => thread
                .snoozed_until
                .clone()
                .flatten()
                .as_deref()
                .and_then(zenith_model::time::millis)
                .map(|until| sidebar_age(now, until))
                .unwrap_or_default()
                .into(),
        };
        let thread_for_menu = thread.clone();
        let hover_id = id.clone();
        let undo_id = id.clone();
        div()
            .id(SharedString::from(format!("row-{}", id.as_str())))
            .h(px(36.))
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(10.))
            .rounded(px(radius::MD))
            .cursor_pointer()
            .when(active, |this| this.bg(c.sidebar_row_active))
            .when(!active, |this| this.hover(|s| s.bg(c.hover)))
            .on_hover(cx.listener(move |this, hovering: &bool, _, cx| {
                if *hovering || this.hovered.as_ref() == Some(&hover_id) {
                    this.hovered = hovering.then(|| hover_id.clone());
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(SidebarEvent::OpenThread(id.clone()))))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| this.open_thread_menu(&thread_for_menu, event.position, window, cx)),
            )
            .child(div().flex_none().when(!lit, |this| this.opacity(0.4)).child(project_badge(&project, 16., cx)))
            .child(
                // A block at full width inside, so GPUI knows the width and adds the ellipsis.
                div().flex_1().min_w_0().child(
                    div()
                        .w_full()
                        .one_line()
                        .text_size(px(14.))
                        .line_height(px(20.))
                        .text_color(if lit { c.text } else { dim })
                        .child(thread.title.clone()),
                ),
            )
            .children(shell::pull_request_badge(thread).map(|badge| pull_request_badge(&badge, cx)))
            .child(
                div()
                    .h(px(24.))
                    .min_w(px(32.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_end()
                    .text_size(px(12.))
                    .line_height(px(16.))
                    .when(!hovered, |this| {
                        this.text_color(if section == Section::Snoozed { c.info_text } else { dim }).child(time)
                    })
                    .when(hovered, |this| {
                        let (glyph, name) = if section == Section::Snoozed {
                            (Icon::AlarmClockOff, "thread.wake")
                        } else {
                            (Icon::Undo2, "thread.reopen")
                        };
                        this.child(
                            div()
                                .id("slim-action")
                                .h_full()
                                .flex()
                                .items_center()
                                .px(px(6.))
                                .mr(px(-4.))
                                .rounded(px(radius::MD))
                                .child(svg().path(glyph.path()).size(px(14.)).text_color(c.text_2))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.command(name, json!({"threadId": undo_id.as_str()}), cx);
                                })),
                        )
                    }),
            )
            .into_any_element()
    }

    fn open_snooze_menu(&mut self, thread: &OrchestrationThreadShell, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let tid = thread.id.as_str().to_owned();
        let entries = snooze_presets()
            .into_iter()
            .map(|(label, until)| {
                let this = this.clone();
                let params = json!({"threadId": tid, "until": until});
                Entry::item(label, move |_, cx| {
                    let _ = this.update(cx, |s, cx| s.command("thread.snooze", params.clone(), cx));
                })
                .icon(Icon::Clock)
            })
            .collect();
        self.menu = Some(OpenMenu::new(entries, position, window, cx, |this, _, _| this.menu = None));
        cx.notify();
    }
}

/// The status the web shows on a card's first line (`resolveSidebarThreadStatus`): its icon
/// (16 px) and label at 12 px, medium, 4 px apart; Working also counts its time.
fn status_label(thread: &OrchestrationThreadShell, status: ThreadStatus, now: i64, cx: &App) -> Option<gpui::Div> {
    let theme = cx.theme();
    let (glyph, label, color) = match status {
        ThreadStatus::Approval => (Icon::ShieldQuestion, "Approval", theme.colors.warning_text),
        ThreadStatus::Input => (Icon::MessageCircleQuestion, "Input", theme.tw("indigo", 600, 300, 1.)),
        ThreadStatus::Working => (Icon::CircleDashed, "Working", theme.tw("sky", 600, 400, 1.)),
        ThreadStatus::Failed => (Icon::CircleAlert, "Failed", theme.tw("red", 700, 300, 1.)),
        ThreadStatus::Monitoring => (
            Icon::Eye,
            "Monitoring",
            if theme.mode == crate::theme::Mode::Dark {
                gpui::white()
            } else {
                theme.colors.text
            },
        ),
        ThreadStatus::PlanReady | ThreadStatus::Ready => return None,
    };
    let elapsed = (status == ThreadStatus::Working)
        .then(|| shell::working_since(thread).map(|since| working_label(now - since)))
        .flatten();
    Some(
        div()
            .flex()
            .items_center()
            .gap(px(4.))
            .text_size(px(12.))
            .line_height(px(16.))
            .font_weight(FontWeight::MEDIUM)
            .text_color(color)
            .child(svg().path(glyph.path()).size(px(16.)).flex_none().text_color(color))
            .child(label)
            .children(elapsed.map(SharedString::from)),
    )
}

/// A branch cut in its middle (`MiddleTruncate`): the last path segment (up to 16 characters)
/// stays whole. GPUI truncates at the end, so long names keep their head and that tail.
fn middle_truncate(branch: &str) -> String {
    const MAX: usize = 28;
    let chars: Vec<char> = branch.chars().collect();
    if chars.len() <= MAX {
        return branch.to_owned();
    }
    let tail_len = branch.rsplit('/').next().map(|t| t.chars().count()).filter(|n| *n <= 16).unwrap_or(10);
    let head: String = chars[..MAX - tail_len - 1].iter().collect();
    let tail: String = chars[chars.len() - tail_len..].iter().collect();
    format!("{head}…{tail}")
}

/// Starts renaming a thread where its menu was opened.
pub type RenameThread = Rc<dyn Fn(&mut Window, &mut App)>;
/// Runs after a thread is archived or deleted from its menu.
pub type ThreadRemoved = Rc<dyn Fn(&mut App)>;

/// What can be done with a thread, in the web's order (`threadActionMenu.logic.ts`): from its
/// row in the sidebar and from its title in the header. `rename` starts renaming it where the
/// menu was opened; `removed` runs after it is archived or deleted.
pub fn thread_menu_entries(thread: &OrchestrationThreadShell, cx: &App, rename: RenameThread, removed: ThreadRemoved) -> Vec<Entry> {
    let store = store::store(cx);
    let now = now_millis();
    let section = shell::section(thread, now);
    let tid = thread.id.as_str().to_owned();
    let cmd = |name: &'static str, params: serde_json::Value| {
        let store = store.clone();
        move |_: &mut Window, cx: &mut App| {
            store.update(cx, |s, cx| s.run_command(name, params.clone(), cx)).detach();
        }
    };
    let on_thread = json!({"threadId": tid});
    let mut entries = Vec::new();
    entries.push(
        Entry::item(
            if section == Section::Pinned { "Unpin thread" } else { "Pin thread" },
            cmd(if section == Section::Pinned { "thread.unpin" } else { "thread.pin" }, on_thread.clone()),
        )
        .icon(if section == Section::Pinned { Icon::PinOff } else { Icon::Pin }),
    );
    entries.push(
        Entry::item(
            if section == Section::Settled { "Un-settle thread" } else { "Settle thread" },
            cmd(if section == Section::Settled { "thread.reopen" } else { "thread.settle" }, on_thread.clone()),
        )
        .icon(Icon::CircleCheck)
        .keys("⌘⇧S"),
    );
    if section == Section::Snoozed {
        entries.push(Entry::item("Wake thread", cmd("thread.wake", on_thread.clone())).icon(Icon::AlarmClockOff));
    } else if shell::can_snooze(thread) {
        entries.push(Entry::Header("Snooze".into()));
        for (label, until) in snooze_presets() {
            entries.push(Entry::item(label, cmd("thread.snooze", json!({"threadId": tid, "until": until}))).icon(Icon::Clock));
        }
    }
    entries.push(Entry::Separator);
    entries.push(Entry::item("Rename thread", move |window, cx| rename(window, cx)).icon(Icon::Pencil));
    entries.push(Entry::item("Regenerate title", cmd("thread.regenerateTitle", on_thread.clone())).icon(Icon::Refresh));
    entries.push(Entry::Separator);
    let path = thread
        .worktree_path
        .clone()
        .or_else(|| store.read(cx).shell.project(&thread.project_id).map(|p| p.workspace_root.clone()));
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
    entries.push(Entry::item("Project settings", |window, cx| window.dispatch_action(Box::new(actions::OpenSettings), cx)).icon(Icon::Settings));
    entries.push(Entry::Separator);
    let running = matches!(shell::status(thread), ThreadStatus::Working);
    {
        let store = store.clone();
        let removed = removed.clone();
        let tid = tid.clone();
        entries.push(
            Entry::item("Archive thread", move |_, cx| {
                store.update(cx, |s, cx| s.run_command("thread.archive", json!({"threadId": tid}), cx)).detach();
                removed(cx);
            })
            .icon(Icon::Archive)
            .disabled(running),
        );
    }
    {
        let title = thread.title.clone();
        entries.push(
            Entry::item("Delete", move |window, cx| {
                let answer = window.prompt(
                    PromptLevel::Warning,
                    &format!("Delete “{title}”?"),
                    Some("The thread and its messages are deleted. Its worktree, if any, stays on disk."),
                    &["Delete Thread", "Cancel"],
                    cx,
                );
                let store = store.clone();
                let removed = removed.clone();
                let tid = tid.clone();
                cx.spawn(async move |cx| {
                    if answer.await == Ok(0) {
                        let _ = cx.update(|cx| {
                            if running {
                                store.update(cx, |s, cx| s.run_command("thread.stop", json!({"threadId": tid}), cx)).detach();
                            }
                            store.update(cx, |s, cx| s.run_command("thread.delete", json!({"threadId": tid}), cx)).detach();
                            removed(cx);
                        });
                    }
                })
                .detach();
            })
            .icon(Icon::Trash)
            .danger(),
        );
    }
    entries
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let c = theme.colors.clone();
        let now = now_millis();
        let query = self.search.read(cx).text().to_owned();
        let store = self.store.read(cx);
        let sections: Vec<(Section, Vec<OrchestrationThreadShell>)> = store
            .shell
            .sidebar(self.project.as_ref(), &query, now)
            .into_iter()
            .map(|(section, threads)| (section, threads.into_iter().cloned().collect()))
            .collect();
        let scoped = self.project.as_ref().and_then(|p| store.shell.project(p)).map(|p| p.title.clone());
        let has_projects = !store.shell.projects_sorted().is_empty();
        // The server's machine, in the web's environment pill, when it is not this one.
        let machine = store.is_remote().then(|| store.server_name());
        let connected = store.connected();
        let shell_loaded = store.shell.loaded;
        let empty = sections.iter().all(|(_, threads)| threads.is_empty());
        let content_left = content_left(window);

        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        let mut shelves: Vec<gpui::AnyElement> = Vec::new();
        for (section, threads) in &sections {
            match section {
                Section::Pinned | Section::Active => {
                    for thread in threads {
                        rows.push(self.render_row(thread, now, cx));
                    }
                }
                Section::Snoozed | Section::Settled => {
                    let expanded = !self.collapsed.contains(section) || !query.is_empty();
                    let section_copy = *section;
                    let snoozed = *section == Section::Snoozed;
                    let (label_color, rule) = if snoozed {
                        (c.info_text, c.info.opacity(0.2))
                    } else {
                        (c.text_2.opacity(0.6), c.line.opacity(c.line.a * 0.6))
                    };
                    let name = if snoozed { "Snoozed" } else { "Settled" };
                    shelves.push(
                        div()
                            .id(SharedString::from(format!("section-{}", section.as_str())))
                            .mx(px(2.))
                            .h(px(32.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .px(px(8.))
                            .cursor_pointer()
                            .text_size(px(12.))
                            .line_height(px(16.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(label_color)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.collapsed.remove(&section_copy) {
                                    this.collapsed.insert(section_copy);
                                }
                                cx.notify();
                            }))
                            .child(SharedString::from(if expanded {
                                name.to_owned()
                            } else {
                                format!("{name} ({})", threads.len())
                            }))
                            .child(div().h(px(1.)).min_w(px(8.)).flex_1().bg(rule))
                            .child(
                                svg()
                                    .path(if expanded { Icon::ChevronUp.path() } else { Icon::ChevronDown.path() })
                                    .size(px(12.))
                                    .flex_none()
                                    .text_color(label_color),
                            )
                            .into_any_element(),
                    );
                    for thread in threads {
                        // Collapsed, the open thread still shows under its shelf.
                        if expanded || self.selected.as_ref() == Some(&thread.id) {
                            shelves.push(self.render_slim_row(thread, now, cx));
                        }
                    }
                }
            }
        }
        // The settled shelf sits at the bottom when the list is short (`mt-auto`).
        let settled_missing = !sections.iter().any(|(s, _)| *s == Section::Settled);
        if settled_missing && shell_loaded {
            shelves.push(
                div()
                    .mx(px(2.))
                    .h(px(32.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(8.))
                    .text_size(px(12.))
                    .line_height(px(16.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(c.text_2.opacity(0.6))
                    .child("Settled (0)")
                    .child(div().h(px(1.)).min_w(px(8.)).flex_1().bg(c.line.opacity(c.line.a * 0.6)))
                    .child(svg().path(Icon::ChevronDown.path()).size(px(12.)).flex_none().text_color(c.text_2.opacity(0.6)))
                    .into_any_element(),
            );
        }
        let icon_color = c.sidebar_icon;
        let hover = c.hover;

        div()
            .size_full()
            .flex()
            .flex_col()
            .font_family(crate::assets::UI_FONT)
            // The brand row; the sidebar's toggle floats over it (see the workspace).
            .child(
                title_bar("sidebar-title-bar")
                    .h(px(TOPBAR))
                    .gap(px(8.))
                    .child(
                        div()
                            .id("brand")
                            .ml(px(content_left))
                            .h(px(28.))
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(px(4.))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| cx.emit(SidebarEvent::NewThread(this.project.clone()))))
                            .child(svg().path("zenith-mark.svg").size(px(16.)).flex_none().text_color(c.text))
                            .child(
                                // The web trims the word to its capitals and centers those
                                // (`text-box: trim-both cap alphabetic`): 3 px above a plain line.
                                div()
                                    .relative()
                                    .top(px(-3.))
                                    .text_size(px(14.))
                                    .line_height(px(20.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(c.text_2)
                                    .child("zenith"),
                            ),
                    )
                    .when_some(machine, |this, machine| {
                        this.child(
                            div()
                                .id("environment-pill")
                                .ml(px(4.))
                                .h(px(16.))
                                .min_w(px(16.))
                                .px(px(3.))
                                .flex()
                                .items_center()
                                .rounded(px(4.))
                                .border_1()
                                .border_color(gpui::transparent_black())
                                .bg(c.muted)
                                .text_size(px(10.))
                                .line_height(px(10.))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(if connected { c.text } else { c.warning_text })
                                .tooltip(move |_, cx| Tooltip::view(if connected { "The server this window drives" } else { "Offline" }, None, cx))
                                .child(machine),
                        )
                    }),
            )
            .map(|this| match self.settings {
                Some(page) => this.child(self.render_settings_nav(page, cx)),
                None => this
                    .child(
                        div()
                            .p(px(8.))
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .child(
                                div()
                                    .id("search")
                                    .flex_1()
                                    .min_w_0()
                                    .h(px(32.))
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .px(px(8.))
                                    .rounded(px(radius::MD))
                                    .hover(move |s| s.bg(hover))
                                    .child(svg().path(Icon::Search.path()).size(px(16.)).flex_none().text_color(icon_color))
                                    .child(div().flex_1().min_w_0().font_weight(FontWeight::MEDIUM).child(self.search.clone())),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_none()
                                    .items_center()
                                    .when(has_projects, |this| {
                                        this.child(
                                            icon_button("filter-projects", Icon::Folder, 28., 16., radius::MD, icon_color, hover, c.text)
                                                .tooltip(move |_, cx| {
                                                    Tooltip::view(
                                                        scoped.clone().map(|p| format!("Showing {p}")).unwrap_or_else(|| "Filter by project".into()),
                                                        None,
                                                        cx,
                                                    )
                                                })
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(|this, event: &MouseDownEvent, window, cx| this.open_project_menu(event.position, window, cx)),
                                                ),
                                        )
                                        .child(
                                            icon_button("new-project", Icon::FolderPlus, 28., 16., radius::MD, icon_color, hover, c.text)
                                                .tooltip(|_, cx| Tooltip::view("New project", Some("⌘O".into()), cx))
                                                .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::AddProject))),
                                        )
                                    })
                                    .child(
                                        icon_button("new-thread", Icon::NewThread, 28., 16., radius::MD, icon_color, hover, c.text)
                                            .when(!has_projects, |this| this.opacity(0.64))
                                            .tooltip(|_, cx| Tooltip::view("New thread", Some("⌘N".into()), cx))
                                            .on_click(cx.listener(|this, _, _, cx| cx.emit(SidebarEvent::NewThread(this.project.clone())))),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .id("threads")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .px(px(8.))
                            .pb(px(8.))
                            .gap(px(1.))
                            .children(rows)
                            .when(empty && shell_loaded, |this| {
                                this.child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .items_center()
                                        .gap(px(8.))
                                        .px(px(8.))
                                        .py(px(24.))
                                        .text_size(px(12.))
                                        .line_height(px(16.))
                                        .text_color(c.text_2.opacity(0.6))
                                        .child(if !has_projects {
                                            "No projects yet".to_owned()
                                        } else if !query.is_empty() {
                                            "No threads found".to_owned()
                                        } else {
                                            match &self.project.as_ref().and_then(|p| self.store.read(cx).shell.project(p)) {
                                                Some(p) => format!("No threads in {} yet", p.title),
                                                None => "No threads yet".to_owned(),
                                            }
                                        }),
                                )
                            })
                            // The shelves sit at the bottom while the list is short (`mt-auto`).
                            .child(div().flex_1())
                            .children(shelves),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .px(px(8.))
                            .py(px(4.))
                            .child(
                                icon_button("open-settings", Icon::Settings, 32., 16., radius::MD, icon_color, hover, c.text)
                                    .tooltip(|_, cx| Tooltip::view("Settings", Some("⌘,".into()), cx))
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::OpenSettings))),
                            )
                            .child(
                                icon_button("open-pull-requests", Icon::PullRequestArrow, 32., 16., radius::MD, icon_color, hover, c.text)
                                    .tooltip(|_, cx| Tooltip::view("Pull Requests", None, cx))
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::OpenPullRequests))),
                            )
                            .child(
                                icon_button("open-usage", Icon::ChartNoAxesColumn, 32., 16., radius::MD, icon_color, hover, c.text)
                                    .tooltip(|_, cx| Tooltip::view("Usage", Some("⌘U".into()), cx))
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::OpenSessions))),
                            )
                            .child(
                                icon_button("open-sessions", Icon::History, 32., 16., radius::MD, icon_color, hover, c.text)
                                    .tooltip(|_, cx| Tooltip::view("Sessions", None, cx))
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::OpenSessions))),
                            ),
                    ),
            })
            .when_some(self.menu.as_ref(), |this, menu| this.child(menu.render()))
    }
}

/// The top bars' height (`--workspace-topbar-height`).
pub const TOPBAR: f32 = 52.;

/// Where the window's own controls end (`--workspace-controls-left`): the traffic lights on
/// macOS, outside full screen; the edge elsewhere.
pub fn controls_left(window: &Window) -> f32 {
    if cfg!(target_os = "macos") && !window.is_fullscreen() {
        90.
    } else {
        12.
    }
}

/// Where a top bar's content starts, past the controls and the sidebar's toggle
/// (`--workspace-titlebar-content-left`: controls, 28 px toggle, 12 px gap).
pub fn content_left(window: &Window) -> f32 {
    controls_left(window) + 28. + 12.
}
