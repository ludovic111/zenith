//! The window: the sidebar (glass) beside the page on screen (a thread, a new thread,
//! settings, sessions), the command palette over them, and notices in a corner.

use gpui::prelude::*;
use gpui::{
    div, px, AnyElement, App, ClickEvent, Context, Entity, FocusHandle, Focusable, FontWeight, MouseButton, MouseDownEvent, PathPromptOptions, SharedString,
    Subscription, Window,
};
use serde_json::json;
use zc_contracts::{ProjectId, ThreadId};
use zenith_model::shell::{self, Section};
use zenith_model::time::now_millis;

use crate::actions;
use crate::assets::Icon;
use crate::folder_picker::{FolderPicker, FolderPickerEvent};
use crate::palette::{Palette, PaletteEvent};
use crate::prefs::Prefs;
use crate::sessions::SessionsView;
use crate::settings::SettingsView;
use crate::sidebar::{Sidebar, SidebarEvent};
use crate::store::{self, Store};
use crate::theme::{text, ActiveTheme, Appearance, Theme};
use crate::thread_view::{ThreadView, ThreadViewEvent};
use crate::ui::{icon, Button};

/// The height of the title bars (the traffic lights sit in the sidebar's).
pub const TITLE_BAR: f32 = 52.;
/// Room left for the traffic lights when the sidebar is hidden.
pub const TRAFFIC_LIGHTS: f32 = 78.;

#[derive(Clone, Debug, PartialEq)]
pub enum Route {
    Home,
    Thread(ThreadId),
    NewThread(ProjectId),
    Settings,
    Sessions,
}

pub struct Workspace {
    prefs: Prefs,
    store: Entity<Store>,
    sidebar: Entity<Sidebar>,
    route: Route,
    threads: Vec<(ThreadId, Entity<ThreadView>)>,
    draft: Option<Entity<ThreadView>>,
    settings: Option<Entity<SettingsView>>,
    sessions: Option<Entity<SessionsView>>,
    palette: Option<(Entity<Palette>, Subscription)>,
    folder_picker: Option<(Entity<FolderPicker>, Subscription)>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(prefs: Prefs, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = store::store(cx);
        let sidebar = cx.new(|cx| Sidebar::new(window, cx));
        let mut subscriptions = vec![
            cx.subscribe_in(&sidebar, window, Self::on_sidebar_event),
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.observe_window_appearance(window, |this, window, cx| this.apply_appearance(window, cx)),
        ];
        // Leaving the window hides the app instead of closing it: the Dock icon brings it back.
        window.on_window_should_close(cx, |_, cx| {
            cx.hide();
            false
        });
        subscriptions.push(cx.observe(&sidebar, |_, _, cx| cx.notify()));
        let route = prefs
            .last_thread
            .clone()
            .map(|id| Route::Thread(ThreadId::from(id.as_str())))
            .unwrap_or(Route::Home);
        let mut this = Self {
            prefs,
            store,
            sidebar,
            route: Route::Home,
            threads: Vec::new(),
            draft: None,
            settings: None,
            sessions: None,
            palette: None,
            folder_picker: None,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        };
        this.apply_appearance(window, cx);
        this.navigate(route, window, cx);
        // `zenith --page settings|sessions|new|palette` opens there (handy from scripts).
        let args: Vec<String> = std::env::args().collect();
        if let Some(page) = args.iter().position(|a| a == "--page").and_then(|i| args.get(i + 1)).cloned() {
            let workspace = cx.entity().downgrade();
            window.defer(cx, move |window, cx| {
                let _ = workspace.update(cx, |this, cx| match page.as_str() {
                    "settings" => this.navigate(Route::Settings, window, cx),
                    "sessions" => this.navigate(Route::Sessions, window, cx),
                    "new" => this.new_thread(None, window, cx),
                    "palette" => this.open_palette(&actions::OpenPalette, window, cx),
                    // The thread on screen with its git menu open.
                    "git" => {
                        if let Some(view) = this.thread_view() {
                            let at = gpui::point(window.viewport_size().width - px(40.), px(40.));
                            view.update(cx, |view, cx| view.open_git_menu(at, window, cx));
                        }
                    }
                    // The thread on screen with its terminal open.
                    "terminal" => {
                        if let Some(view) = this.thread_view() {
                            view.update(cx, |view, cx| view.toggle_terminal(window, cx));
                        }
                    }
                    _ => {}
                });
            });
        }
        this
    }

    fn apply_appearance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mode = Theme::mode_for(self.prefs.appearance, window.appearance());
        let reduce = cx.theme().reduce_transparency;
        if cx.theme().mode != mode {
            cx.set_global(Theme::new(mode, reduce));
            cx.refresh_windows();
        }
    }

    pub fn set_appearance(&mut self, appearance: Appearance, window: &mut Window, cx: &mut Context<Self>) {
        self.prefs.appearance = appearance;
        self.prefs.save();
        self.apply_appearance(window, cx);
        cx.notify();
    }

    pub fn appearance(&self) -> Appearance {
        self.prefs.appearance
    }

    pub fn prefs(&self) -> &Prefs {
        &self.prefs
    }

    pub fn set_check_for_updates(&mut self, on: bool, cx: &mut Context<Self>) {
        self.prefs.check_for_updates = on;
        self.prefs.save();
        cx.notify();
    }

    /// Shows a page; threads stay open (and subscribed) for a quick return.
    pub fn navigate(&mut self, route: Route, window: &mut Window, cx: &mut Context<Self>) {
        match &route {
            Route::Thread(id) => {
                // Subscribed again if the store let it go while the view stayed cached.
                self.store.update(cx, |store, cx| store.open_thread(id, cx));
                if !self.threads.iter().any(|(t, _)| t == id) {
                    let view = cx.new(|cx| ThreadView::existing(id.clone(), window, cx));
                    self._subscriptions.push(cx.subscribe_in(&view, window, Self::on_thread_event));
                    self.threads.push((id.clone(), view));
                    if self.threads.len() > 8 {
                        self.threads.remove(0);
                    }
                }
                self.prefs.last_thread = Some(id.as_str().to_owned());
                self.prefs.save();
                self.sidebar.update(cx, |s, cx| s.set_selected(Some(id.clone()), cx));
            }
            Route::NewThread(project) => {
                let reuse = self.draft.as_ref().is_some_and(|d| d.read(cx).project_id(cx).as_ref() == Some(project));
                if !reuse {
                    let view = cx.new(|cx| ThreadView::draft(project.clone(), window, cx));
                    self._subscriptions.push(cx.subscribe_in(&view, window, Self::on_thread_event));
                    self.draft = Some(view);
                }
                self.sidebar.update(cx, |s, cx| s.set_selected(None, cx));
            }
            Route::Settings => {
                if self.settings.is_none() {
                    let workspace = cx.entity().downgrade();
                    self.settings = Some(cx.new(|cx| SettingsView::new(workspace, window, cx)));
                }
                self.sidebar.update(cx, |s, cx| s.set_selected(None, cx));
            }
            Route::Sessions => {
                if self.sessions.is_none() {
                    self.sessions = Some(cx.new(|cx| SessionsView::new(window, cx)));
                }
                if let Some(s) = self.sessions.as_ref() {
                    s.update(cx, |s, cx| s.refresh(cx))
                }
                self.sidebar.update(cx, |s, cx| s.set_selected(None, cx));
            }
            Route::Home => {
                self.sidebar.update(cx, |s, cx| s.set_selected(None, cx));
            }
        }
        self.route = route;
        self.focus_page(window, cx);
        cx.notify();
    }

    fn focus_page(&self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.route {
            Route::Thread(id) => {
                if let Some((_, view)) = self.threads.iter().find(|(t, _)| t == id) {
                    view.update(cx, |v, cx| v.focus_composer(window, cx));
                }
            }
            Route::NewThread(_) => {
                if let Some(view) = &self.draft {
                    view.update(cx, |v, cx| v.focus_composer(window, cx));
                }
            }
            _ => window.focus(&self.focus_handle),
        }
    }

    /// The thread view on screen (a thread, or a draft).
    pub fn thread_view(&self) -> Option<Entity<ThreadView>> {
        match &self.route {
            Route::Thread(id) => self.threads.iter().find(|(t, _)| t == id).map(|(_, v)| v.clone()),
            Route::NewThread(_) => self.draft.clone(),
            _ => None,
        }
    }

    fn git_action(&mut self, action: &'static str, cx: &mut Context<Self>) {
        if let Some(view) = self.thread_view() {
            view.update(cx, |view, cx| view.git_action(action, cx));
        }
    }

    fn current_thread(&self) -> Option<ThreadId> {
        match &self.route {
            Route::Thread(id) => Some(id.clone()),
            _ => None,
        }
    }

    /// The project a new thread goes in: the current thread's, the sidebar's filter, the most
    /// recently active.
    fn default_project(&self, cx: &App) -> Option<ProjectId> {
        let store = self.store.read(cx);
        if let Some(id) = self.current_thread() {
            if let Some(thread) = store.shell.thread(&id) {
                return Some(thread.project_id.clone());
            }
        }
        if let Route::NewThread(project) = &self.route {
            return Some(project.clone());
        }
        if let Some(project) = self.sidebar.read(cx).project_filter() {
            return Some(project);
        }
        store
            .shell
            .threads
            .iter()
            .max_by_key(|t| shell::activity_at(t))
            .map(|t| t.project_id.clone())
            .or_else(|| store.shell.projects_sorted().first().map(|p| p.id.clone()))
    }

    pub fn new_thread(&mut self, project: Option<ProjectId>, window: &mut Window, cx: &mut Context<Self>) {
        match project.or_else(|| self.default_project(cx)) {
            Some(project) => self.navigate(Route::NewThread(project), window, cx),
            None => self.add_project(window, cx),
        }
    }

    /// Asks for a folder and adds it as a project: in the Mac's folder picker, or among the
    /// server's folders when it is on another machine.
    pub fn add_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.store.read(cx).is_remote() {
            self.open_folder_picker(window, cx);
            return;
        }
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add Project".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let _ = this.update_in(cx, |this, window, cx| this.add_project_at(path.to_string_lossy().into_owned(), window, cx));
        })
        .detach();
    }

    fn add_project_at(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let params = json!({"path": path});
        let task = self.store.update(cx, |store, cx| store.run_command("project.add", params, cx));
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(added) = task.await {
                let Some(project_id) = added.get("projectId").and_then(|v| v.as_str()).map(ProjectId::from) else {
                    return;
                };
                let _ = this.update_in(cx, |this, window, cx| this.navigate(Route::NewThread(project_id), window, cx));
            }
        })
        .detach();
    }

    fn open_folder_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = None;
        let picker = cx.new(|cx| FolderPicker::new(window, cx));
        let subscription = cx.subscribe_in(&picker, window, |this, _, event: &FolderPickerEvent, window, cx| {
            this.folder_picker = None;
            match event {
                FolderPickerEvent::Dismissed => this.focus_page(window, cx),
                FolderPickerEvent::Picked(path) => this.add_project_at(path.clone(), window, cx),
            }
            cx.notify();
        });
        picker.update(cx, |p, cx| p.focus(window, cx));
        self.folder_picker = Some((picker, subscription));
        cx.notify();
    }

    fn on_sidebar_event(&mut self, _: &Entity<Sidebar>, event: &SidebarEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            SidebarEvent::OpenThread(id) => self.navigate(Route::Thread(id.clone()), window, cx),
            SidebarEvent::NewThread(project) => self.new_thread(project.clone(), window, cx),
            SidebarEvent::AddProject => self.add_project(window, cx),
            SidebarEvent::OpenSettings => self.navigate(Route::Settings, window, cx),
            SidebarEvent::OpenSessions => self.navigate(Route::Sessions, window, cx),
            SidebarEvent::ToggleSidebar => self.toggle_sidebar(&actions::ToggleSidebar, window, cx),
            SidebarEvent::Removed(id) => self.left_thread(id, window, cx),
        }
    }

    fn on_thread_event(&mut self, view: &Entity<ThreadView>, event: &ThreadViewEvent, _: &mut Window, cx: &mut Context<Self>) {
        match event {
            ThreadViewEvent::Created(id) => {
                // The draft became a real thread: keep its view, as that thread's.
                if self.draft.as_ref() == Some(view) {
                    self.draft = None;
                    self.threads.push((id.clone(), view.clone()));
                }
                self.route = Route::Thread(id.clone());
                self.prefs.last_thread = Some(id.as_str().to_owned());
                self.prefs.save();
                self.sidebar.update(cx, |s, cx| s.set_selected(Some(id.clone()), cx));
                cx.notify();
            }
        }
    }

    /// The thread on screen was archived or deleted: show the next one.
    fn left_thread(&mut self, id: &ThreadId, window: &mut Window, cx: &mut Context<Self>) {
        self.threads.retain(|(t, _)| t != id);
        if self.current_thread().as_ref() == Some(id) {
            let next = self.sidebar.read(cx).neighbor(id, 1, cx).or_else(|| self.sidebar.read(cx).neighbor(id, -1, cx));
            match next {
                Some(next) => self.navigate(Route::Thread(next), window, cx),
                None => self.navigate(Route::Home, window, cx),
            }
        }
    }

    fn toggle_sidebar(&mut self, _: &actions::ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.prefs.sidebar_visible = !self.prefs.sidebar_visible;
        self.prefs.save();
        cx.notify();
    }

    fn open_palette(&mut self, _: &actions::OpenPalette, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            self.palette = None;
            self.focus_page(window, cx);
            cx.notify();
            return;
        }
        let context = crate::palette::PaletteContext {
            thread: self.current_thread(),
            project: self.default_project(cx),
        };
        let palette = cx.new(|cx| Palette::new(context, window, cx));
        let subscription = cx.subscribe_in(&palette, window, |this, _, event: &PaletteEvent, window, cx| {
            this.palette = None;
            match event {
                PaletteEvent::Dismissed => this.focus_page(window, cx),
                PaletteEvent::Navigate(route) => this.navigate(route.clone(), window, cx),
                PaletteEvent::NewThread(project) => this.new_thread(project.clone(), window, cx),
                PaletteEvent::AddProject => this.add_project(window, cx),
                PaletteEvent::Appearance(appearance) => this.set_appearance(*appearance, window, cx),
                PaletteEvent::Action(action) => window.dispatch_action(action.boxed_clone(), cx),
                PaletteEvent::Script(id) => {
                    if let Some(view) = this.thread_view() {
                        view.update(cx, |view, cx| view.run_script(id.clone(), window, cx));
                    }
                }
            }
            cx.notify();
        });
        palette.update(cx, |p, cx| p.focus(window, cx));
        self.palette = Some((palette, subscription));
        cx.notify();
    }

    fn step_thread(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let sidebar = self.sidebar.read(cx);
        let next = match self.current_thread() {
            Some(id) => sidebar.neighbor(&id, delta, cx),
            None => sidebar.first(cx),
        };
        if let Some(next) = next {
            self.navigate(Route::Thread(next), window, cx);
        }
    }

    fn thread_action(&mut self, kind: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.current_thread() else { return };
        let store = self.store.clone();
        let Some(thread) = store.read(cx).shell.thread(&id).cloned() else { return };
        let section = shell::section(&thread, now_millis());
        let name = match kind {
            "pin" if section == Section::Pinned => "thread.unpin",
            "pin" => "thread.pin",
            "settle" if section == Section::Settled => "thread.reopen",
            "settle" => "thread.settle",
            "archive" => "thread.archive",
            _ => return,
        };
        let params = json!({"threadId": id.as_str()});
        store.update(cx, |store, cx| store.run_command(name, params, cx)).detach();
        if kind == "archive" {
            self.left_thread(&id, window, cx);
        }
    }

    fn open_in_browser(&mut self, _: &actions::OpenInBrowser, _: &mut Window, cx: &mut Context<Self>) {
        // The browser has no session of its own: it gets a one-time pairing token.
        let base = self.store.read(cx).client.base_url().to_owned();
        if self.store.read(cx).is_remote() {
            // A remote server mints the code itself for this machine's session.
            let client = self.store.read(cx).client.clone();
            let task = crate::runtime::spawn(async move { client.browser_pairing_code("zenith browser").await });
            cx.spawn(async move |this, cx| {
                let url = match task.await {
                    Ok(Ok(code)) => format!("{base}/pair#token={code}"),
                    Ok(Err(error)) => {
                        let message = format!("No pairing code for the browser ({error:#}): sign in there by hand.");
                        let _ = this.update(cx, |this, cx| this.store.update(cx, |s, cx| s.notify_error(message, cx)));
                        format!("{base}/")
                    }
                    Err(_) => format!("{base}/"),
                };
                let _ = cx.update(|cx| cx.open_url(&url));
            })
            .detach();
            return;
        }
        let task = crate::runtime::spawn(async move {
            let binary = zenith_client::local::server_binary();
            let output = tokio::process::Command::new(binary)
                .args([
                    "auth",
                    "pairing",
                    "create",
                    "--ttl",
                    "2m",
                    "--admin",
                    "--label",
                    "zenith browser",
                    "--json",
                    "--base-dir",
                ])
                .arg(zenith_client::local::code_home())
                .output()
                .await
                .ok()?;
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let json: serde_json::Value = serde_json::from_str(&stdout[stdout.find('{')?..]).ok()?;
            json["credential"].as_str().map(String::from)
        });
        cx.spawn(async move |_, cx| {
            let url = match task.await.ok().flatten() {
                Some(token) => format!("{base}/pair#token={token}"),
                None => format!("{base}/"),
            };
            let _ = cx.update(|cx| cx.open_url(&url));
        })
        .detach();
    }

    fn render_title_bar_spacer(&self) -> impl IntoElement {
        div().w(px(if self.prefs.sidebar_visible { 0. } else { TRAFFIC_LIGHTS }))
    }

    fn page(&self, cx: &App) -> AnyElement {
        match &self.route {
            Route::Thread(id) => match self.threads.iter().find(|(t, _)| t == id) {
                Some((_, view)) => view.clone().into_any_element(),
                None => self.home(cx),
            },
            Route::NewThread(_) => match &self.draft {
                Some(view) => view.clone().into_any_element(),
                None => self.home(cx),
            },
            Route::Settings => self.settings.clone().map(|s| s.into_any_element()).unwrap_or_else(|| self.home(cx)),
            Route::Sessions => self.sessions.clone().map(|s| s.into_any_element()).unwrap_or_else(|| self.home(cx)),
            Route::Home => self.home(cx),
        }
    }

    /// Nothing selected: a welcome, or why the server is not there.
    fn home(&self, cx: &App) -> AnyElement {
        let c = cx.theme().colors.clone();
        let store = self.store.read(cx);
        let has_projects = !store.shell.projects.is_empty();
        let (title, body): (SharedString, SharedString) = if !store.connected() && !store.ever_connected {
            (
                "Starting the local server".into(),
                match &store.status {
                    zenith_client::ConnectionStatus::Failed(reason) => {
                        format!("{reason}. zenith keeps trying; the log is ~/Library/Logs/Zenith/server.log.").into()
                    }
                    _ => "Connecting to zenith on this Mac…".into(),
                },
            )
        } else if has_projects {
            (
                "Pick a thread, or start one".into(),
                "⌘N starts a thread in the current project, ⌘K finds anything.".into(),
            )
        } else {
            (
                "Add a project to start".into(),
                "A project is a folder with your code. The agents work in it, or in worktrees of it.".into(),
            )
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(12.))
            .child(
                div()
                    .size(px(56.))
                    .rounded(px(14.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(c.accent_soft)
                    .child(icon(Icon::Sparkles, c.accent_text).size(px(26.))),
            )
            .child(div().text_size(px(text::XL)).font_weight(FontWeight::BOLD).text_color(c.text).child(title))
            .child(div().max_w(px(440.)).text_center().text_size(px(text::BASE)).text_color(c.text_2).child(body))
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .pt(px(8.))
                    .when(has_projects, |this| {
                        this.child(
                            Button::new("home-new-thread")
                                .label("New thread")
                                .icon(Icon::NewThread)
                                .variant(crate::ui::Variant::Primary)
                                .on_click(|_, window, cx| window.dispatch_action(Box::new(actions::NewThread), cx)),
                        )
                    })
                    .child(
                        Button::new("home-add-project")
                            .label("Add project…")
                            .icon(Icon::FolderPlus)
                            .variant(if has_projects {
                                crate::ui::Variant::Secondary
                            } else {
                                crate::ui::Variant::Primary
                            })
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(actions::AddProject), cx)),
                    ),
            )
            .into_any_element()
    }

    fn render_notices(&self, cx: &App) -> impl IntoElement {
        let theme = cx.theme().clone();
        let c = theme.colors.clone();
        let notices = self.store.read(cx).notices.clone();
        let store = self.store.clone();
        div()
            .absolute()
            .bottom(px(16.))
            .right(px(16.))
            .flex()
            .flex_col()
            .gap(px(8.))
            .max_w(px(420.))
            .children(notices.into_iter().map(move |notice| {
                let store = store.clone();
                let id = notice.id;
                div()
                    .id(SharedString::from(format!("notice-{id}")))
                    .flex()
                    .items_start()
                    .gap(px(8.))
                    .px(px(12.))
                    .py(px(10.))
                    .rounded(px(10.))
                    .bg(theme.floating_bg())
                    .border_1()
                    .border_color(if notice.error { c.danger } else { c.glass_edge })
                    .shadow(theme.floating_shadow())
                    .text_size(px(text::BASE))
                    .text_color(c.text)
                    .cursor_pointer()
                    .on_click(move |_: &ClickEvent, _, cx| store.update(cx, |s, cx| s.dismiss_notice(id, cx)))
                    .child(icon(
                        if notice.error { Icon::CircleAlert } else { Icon::Info },
                        if notice.error { c.danger } else { c.accent_text },
                    ))
                    .child(div().flex_1().child(notice.message))
            }))
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// A title bar strip: drags the window, zooms it on a double click.
pub fn title_bar(id: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(TITLE_BAR))
        .flex_none()
        .flex()
        .items_center()
        .on_mouse_down(MouseButton::Left, |event: &MouseDownEvent, window, _| {
            if event.click_count >= 2 {
                window.titlebar_double_click();
            } else {
                window.start_window_move();
            }
        })
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let c = theme.colors.clone();
        let sidebar_visible = self.prefs.sidebar_visible;
        let fullscreen = window.is_fullscreen();
        let opaque_window = theme.reduce_transparency;
        let page = self.page(cx);
        div()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .relative()
            .font_family(crate::assets::UI_FONT)
            .text_color(c.text)
            .text_size(px(text::BASE))
            .when(opaque_window, |this| this.bg(c.bg))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::open_palette))
            .on_action(cx.listener(Self::open_in_browser))
            .on_action(cx.listener(|this, _: &actions::NewThread, window, cx| this.new_thread(None, window, cx)))
            .on_action(cx.listener(|this, _: &actions::AddProject, window, cx| this.add_project(window, cx)))
            .on_action(cx.listener(|this, _: &actions::OpenSettings, window, cx| this.navigate(Route::Settings, window, cx)))
            .on_action(cx.listener(|this, _: &actions::OpenSessions, window, cx| this.navigate(Route::Sessions, window, cx)))
            .on_action(cx.listener(|this, _: &actions::NextThread, window, cx| this.step_thread(1, window, cx)))
            .on_action(cx.listener(|this, _: &actions::PreviousThread, window, cx| this.step_thread(-1, window, cx)))
            .on_action(cx.listener(|this, _: &actions::PinThread, window, cx| this.thread_action("pin", window, cx)))
            .on_action(cx.listener(|this, _: &actions::SettleThread, window, cx| this.thread_action("settle", window, cx)))
            .on_action(cx.listener(|this, _: &actions::ArchiveThread, window, cx| this.thread_action("archive", window, cx)))
            .on_action(cx.listener(|this, _: &actions::FocusComposer, window, cx| this.focus_page(window, cx)))
            .on_action(cx.listener(|this, _: &actions::StopTurn, _, cx| {
                if let Some(view) = this.thread_view() {
                    view.update(cx, |view, cx| view.stop(cx));
                }
            }))
            .on_action(cx.listener(|this, _: &actions::ToggleTerminal, window, cx| {
                if let Some(view) = this.thread_view() {
                    view.update(cx, |view, cx| view.toggle_terminal(window, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &actions::GitCommit, _, cx| this.git_action("commit", cx)))
            .on_action(cx.listener(|this, _: &actions::GitCommitPush, _, cx| this.git_action("commit_push", cx)))
            .on_action(cx.listener(|this, _: &actions::GitCommitPushPr, _, cx| this.git_action("commit_push_pr", cx)))
            .on_action(cx.listener(|this, _: &actions::GitPush, _, cx| this.git_action("push", cx)))
            .on_action(cx.listener(|this, _: &actions::GitPull, _, cx| this.git_action("pull", cx)))
            .on_action(cx.listener(|_, _: &actions::CloseWindow, _, cx| cx.hide()))
            .on_action(cx.listener(|_, _: &actions::Minimize, window, _| window.minimize_window()))
            .on_action(cx.listener(|_, _: &actions::Zoom, window, _| window.zoom_window()))
            .on_action(cx.listener(|_, _: &actions::ToggleFullScreen, window, _| window.toggle_fullscreen()))
            .on_action(cx.listener(|this, _: &actions::ReloadConnection, _, cx| {
                this.store.update(cx, |s, cx| {
                    if s.is_remote() {
                        // Nothing to wake up from here: the connection retries on its own.
                        let message = format!("The server is on {}: zenith reconnects to it by itself", s.server_name());
                        s.notify_info(message, cx);
                    } else {
                        zenith_client::local::kickstart();
                        s.notify_info("Asked the server to start", cx);
                    }
                });
            }))
            .when(sidebar_visible, |this| {
                this.child(
                    div()
                        .w(px(self.prefs.sidebar_width))
                        .h_full()
                        .flex_none()
                        .bg(c.glass_1)
                        .border_r_1()
                        .border_color(theme.hairline())
                        .child(self.sidebar.clone()),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .bg(c.bg_raised)
                    .child(
                        title_bar("page-title-bar")
                            .px(px(12.))
                            .gap(px(8.))
                            .border_b_1()
                            .border_color(c.line)
                            .when(!sidebar_visible && !fullscreen, |this| this.child(self.render_title_bar_spacer()))
                            .when(!sidebar_visible, |this| {
                                this.child(
                                    Button::new("show-sidebar")
                                        .icon(Icon::PanelLeft)
                                        .tooltip_keys("Show the sidebar", "⌘B")
                                        .on_click(|_, window, cx| window.dispatch_action(Box::new(actions::ToggleSidebar), cx)),
                                )
                            })
                            .child(self.page_title(cx)),
                    )
                    .child(div().flex_1().min_h_0().child(page)),
            )
            .child(self.render_notices(cx))
            .when_some(self.palette.as_ref(), |this, (palette, _)| {
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .justify_center()
                        .pt(px(96.))
                        .bg(c.scrim)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                this.palette = None;
                                this.focus_page(window, cx);
                                cx.notify();
                            }),
                        )
                        .child(palette.clone()),
                )
            })
            .when_some(self.folder_picker.as_ref(), |this, (picker, _)| {
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .justify_center()
                        .pt(px(96.))
                        .bg(c.scrim)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                this.folder_picker = None;
                                this.focus_page(window, cx);
                                cx.notify();
                            }),
                        )
                        .child(picker.clone()),
                )
            })
    }
}

impl Workspace {
    /// The page's title in the shared title bar.
    fn page_title(&self, cx: &App) -> AnyElement {
        match &self.route {
            Route::Thread(id) => match self.threads.iter().find(|(t, _)| t == id) {
                Some((_, view)) => view.read(cx).render_header(cx),
                None => div().into_any_element(),
            },
            Route::NewThread(_) => match &self.draft {
                Some(view) => view.read(cx).render_header(cx),
                None => div().into_any_element(),
            },
            Route::Settings => header_text("Settings", cx),
            Route::Sessions => header_text("Sessions & costs", cx),
            Route::Home => div().into_any_element(),
        }
    }
}

pub fn header_text(title: &str, cx: &App) -> AnyElement {
    div()
        .text_size(px(text::MD))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(cx.theme().colors.text)
        .child(SharedString::from(title.to_owned()))
        .into_any_element()
}
