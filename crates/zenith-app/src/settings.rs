//! Settings, in the web's pages (`/settings/*`): General (new threads, organization,
//! behavior, projects, updates), Appearance, Providers, Integrations (lsuite apps, what agents
//! driving zenith may do), Connections (the server this window drives), Archive; the pages
//! the window does not have yet open in the browser. Every change goes through the command
//! registry, like the CLI's.

use gpui::prelude::*;
use gpui::{
    div, px, svg, AnyElement, App, ClipboardItem, Context, Entity, FontWeight, MouseButton, MouseDownEvent, PromptLevel, SharedString, Subscription,
    WeakEntity, Window,
};
use serde_json::{json, Value};
use zc_contracts::OrchestrationThreadShell;
use zenith_commands::permissions::{Access, Permissions};

use crate::assets::{Icon, MONO_FONT};
use crate::composer::{provider_name, runtime_mode_icon, runtime_mode_label, RUNTIME_MODES};
use crate::store::{self, Store};
use crate::theme::{radius, text, ActiveTheme, Appearance};
use crate::ui::menu::{Entry, OpenMenu};
use crate::ui::OneLine;
use crate::ui::{dot, pill, Button, Variant};
use crate::update::{self, UpdateState};
use crate::workspace::Workspace;

/// The web's settings pages, in its order (`SettingsSidebarNav`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SettingsPage {
    #[default]
    General,
    Appearance,
    Keybindings,
    SnapShots,
    Providers,
    Integrations,
    SourceControl,
    Storage,
    Connections,
    Archive,
}

impl SettingsPage {
    pub const ALL: [SettingsPage; 10] = [
        Self::General,
        Self::Appearance,
        Self::Keybindings,
        Self::SnapShots,
        Self::Providers,
        Self::Integrations,
        Self::SourceControl,
        Self::Storage,
        Self::Connections,
        Self::Archive,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Appearance => "Appearance",
            Self::Keybindings => "Keybindings",
            Self::SnapShots => "SnapShots",
            Self::Providers => "Providers",
            Self::Integrations => "Integrations",
            Self::SourceControl => "Source Control",
            Self::Storage => "Storage",
            Self::Connections => "Connections",
            Self::Archive => "Archive",
        }
    }

    pub fn icon(self) -> Icon {
        match self {
            Self::General => Icon::Settings2,
            Self::Appearance => Icon::Palette,
            Self::Keybindings => Icon::Keyboard,
            Self::SnapShots => Icon::Camera,
            Self::Providers => Icon::Bot,
            Self::Integrations => Icon::Blocks,
            Self::SourceControl => Icon::GitBranch,
            Self::Storage => Icon::HardDrive,
            Self::Connections => Icon::Link2,
            Self::Archive => Icon::Archive,
        }
    }

    /// The page on the web (`/settings/…`).
    fn web_path(self) -> &'static str {
        match self {
            Self::General => "/settings/general",
            Self::Appearance => "/settings/appearance",
            Self::Keybindings => "/settings/keybindings",
            Self::SnapShots => "/settings/snap-shot",
            Self::Providers => "/settings/providers",
            Self::Integrations => "/settings/integrations",
            Self::SourceControl => "/settings/source-control",
            Self::Storage => "/settings/storage",
            Self::Connections => "/settings/connections",
            Self::Archive => "/settings/archived",
        }
    }
}

pub struct SettingsView {
    pub page: SettingsPage,
    menu: Option<OpenMenu>,
    store: Entity<Store>,
    workspace: WeakEntity<Workspace>,
    permissions: Permissions,
    archived: Option<Vec<OrchestrationThreadShell>>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsView {
    pub fn new(workspace: WeakEntity<Workspace>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = store::store(cx);
        let updates = update::updates(cx);
        let subscriptions = vec![cx.observe(&store, |_, _, cx| cx.notify()), cx.observe(&updates, |_, _, cx| cx.notify())];
        let mut this = Self {
            page: SettingsPage::General,
            menu: None,
            store,
            workspace,
            permissions: Permissions::load(),
            archived: None,
            _subscriptions: subscriptions,
        };
        this.load_archived(cx);
        this
    }

    fn load_archived(&mut self, cx: &mut Context<Self>) {
        let task = self.store.update(cx, |s, cx| s.call("orchestration.getArchivedShellSnapshot", json!({}), cx));
        cx.spawn(async move |this, cx| {
            if let Ok(value) = task.await {
                let threads = serde_json::from_value::<zc_contracts::OrchestrationShellSnapshot>(value)
                    .map(|s| s.threads)
                    .unwrap_or_default();
                let _ = this.update(cx, |this, cx| {
                    this.archived = Some(threads);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn command(&mut self, name: &'static str, params: Value, cx: &mut Context<Self>) {
        let task = self.store.update(cx, |s, cx| s.run_command(name, params, cx));
        cx.spawn(async move |this, cx| {
            let ok = task.await.is_ok();
            if ok && (name.starts_with("thread.") || name == "project.remove") {
                let _ = this.update(cx, |this, cx| this.load_archived(cx));
            }
        })
        .detach();
    }

    pub fn set_page(&mut self, page: SettingsPage, cx: &mut Context<Self>) {
        self.page = page;
        self.menu = None;
        cx.notify();
    }

    fn update_settings(&mut self, patch: Value, cx: &mut Context<Self>) {
        self.command("settings.update", json!({ "patch": patch }), cx);
    }

    /// A select's menu: `(value, label, icon)`, the current one checked.
    fn open_select(
        &mut self,
        event: &MouseDownEvent,
        options: Vec<(Value, &'static str, Option<Icon>)>,
        current: Value,
        key: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity().downgrade();
        let entries = options
            .into_iter()
            .map(|(value, label, glyph)| {
                let this = this.clone();
                let checked = value == current;
                let entry = Entry::item(label, move |_, cx| {
                    let _ = this.update(cx, |s, cx| s.update_settings(json!({ key: value.clone() }), cx));
                })
                .checked(checked);
                match glyph {
                    Some(glyph) => entry.icon(glyph),
                    None => entry,
                }
            })
            .collect();
        self.menu = Some(OpenMenu::new(entries, event.position, window, cx, |this, _, _| this.menu = None));
        cx.notify();
    }

    fn set_permissions(&mut self, access: Access, cx: &mut Context<Self>) {
        self.permissions.mcp = access;
        let value = match access {
            Access::Off => "off",
            Access::Read => "read",
            Access::Full => "full",
        };
        self.command("settings.setAgentPermissions", json!({"mcp": value}), cx);
        cx.notify();
    }
}

/// A section: its title (14 px at 70% of the text), then its card (`SettingsSection`).
fn section(title: &str, rows: Vec<AnyElement>, cx: &App) -> AnyElement {
    let c = cx.theme().colors.clone();
    let rows_len = rows.len();
    div()
        .flex()
        .flex_col()
        .child(
            div()
                .h(px(28.))
                .px(px(16.))
                .mb(px(10.))
                .flex()
                .items_center()
                .text_size(px(14.))
                .line_height(px(20.))
                .text_color(c.text.opacity(0.7))
                .child(SharedString::from(title.to_owned())),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .rounded(px(radius::XL))
                .border_1()
                .border_color(c.line)
                .bg(c.bg_sunken.opacity(0.4))
                .overflow_hidden()
                .children(
                    rows.into_iter()
                        .enumerate()
                        .map(|(i, row)| div().when(i > 0 && i < rows_len, |el| el.border_t_1().border_color(c.line)).child(row)),
                ),
        )
        .into_any_element()
}

/// A settings row (`SettingsRow`): the label at 14 px medium, its description under it at
/// 12 on 18 in muted text, the control on the right.
fn row(label: impl Into<SharedString>, detail: Option<SharedString>, right: AnyElement, cx: &App) -> gpui::Div {
    let c = cx.theme().colors.clone();
    div()
        .flex()
        .items_center()
        .gap(px(16.))
        .px(px(16.))
        .py(px(12.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .text_size(px(14.))
                        .line_height(px(20.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(c.text)
                        .child(label.into()),
                )
                .when_some(detail, |this, detail| {
                    this.child(div().text_size(px(12.)).line_height(px(18.)).text_color(c.text_2.opacity(0.8)).child(detail))
                }),
        )
        .child(div().flex_none().child(right))
}

/// A select (`Select size="sm"`): 28 px, 10 px corners, on the background with the input's
/// border; its icon, value and chevron.
fn select(id: &'static str, glyph: Option<Icon>, label: impl Into<SharedString>, cx: &App) -> gpui::Stateful<gpui::Div> {
    let c = cx.theme().colors.clone();
    let accent = c.accent_soft.opacity(0.5);
    div()
        .id(id)
        .h(px(28.))
        .min_w(px(if id == "auto-settle-days" { 48. } else { 160. }))
        .px(px(9.))
        .flex()
        .items_center()
        .gap(px(6.))
        .rounded(px(radius::LG))
        .border_1()
        .border_color(c.line_strong)
        .bg(c.bg_raised)
        .cursor_pointer()
        .hover(move |s| s.bg(accent))
        .text_size(px(14.))
        .line_height(px(20.))
        .text_color(c.text)
        .when_some(glyph, |el, glyph| {
            el.child(svg().path(glyph.path()).size(px(14.)).flex_none().text_color(c.text_2))
        })
        .child(div().flex_1().min_w_0().one_line().child(label.into()))
        .child(svg().path(Icon::ChevronDown.path()).size(px(12.)).flex_none().text_color(c.text_3))
}

/// A switch (`Switch`): 30 × 18, the primary color when on, a 14 px knob.
fn switch(id: &'static str, on: bool, cx: &App) -> gpui::Stateful<gpui::Div> {
    let c = cx.theme().colors.clone();
    div()
        .id(id)
        .w(px(30.))
        .h(px(18.))
        .p(px(2.))
        .flex()
        .items_center()
        .when(on, |el| el.justify_end())
        .rounded_full()
        .cursor_pointer()
        .bg(if on { c.accent_fill } else { c.line_strong })
        .child(div().size(px(14.)).rounded_full().bg(c.bg_raised))
}

/// A page the window does not have yet: where to find it.
fn web_only(page: SettingsPage, base: String, cx: &App) -> AnyElement {
    let c = cx.theme().colors.clone();
    let url = format!("{base}{}", page.web_path());
    section(
        page.label(),
        vec![row(
            "In the web interface for now",
            Some(format!("{} settings are not in the window yet; they open in the browser.", page.label()).into()),
            Button::new("open-web-settings")
                .label("Open in the browser")
                .icon(Icon::ExternalLink)
                .small()
                .variant(Variant::Secondary)
                .on_click(move |_, _, cx| cx.open_url(&url))
                .into_any_element(),
            cx,
        )
        .text_color(c.text)
        .into_any_element()],
        cx,
    )
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = cx.theme().colors.clone();
        let page = self.page;
        let base = self.store.read(cx).client.base_url().to_owned();
        let sections: Vec<AnyElement> = match page {
            SettingsPage::General => self.general(cx),
            SettingsPage::Appearance => self.appearance(cx),
            SettingsPage::Providers => self.providers(cx),
            SettingsPage::Integrations => self.integrations(cx),
            SettingsPage::Connections => self.connections(cx),
            SettingsPage::Archive => self.archive(cx),
            other => vec![web_only(other, base, cx)],
        };
        div()
            .size_full()
            .child(
                div().id("settings-scroll").size_full().overflow_y_scroll().child(
                    div().flex().justify_center().px(px(20.)).pt(px(24.)).pb(px(40.)).child(
                        div()
                            .w_full()
                            .max_w(px(848.))
                            .flex()
                            .flex_col()
                            .gap(px(32.))
                            .when(page == SettingsPage::General, |el| {
                                el.child(
                                    div()
                                        .px(px(16.))
                                        .flex()
                                        .items_center()
                                        .gap(px(6.))
                                        .text_size(px(16.))
                                        .line_height(px(24.))
                                        .text_color(c.text_2)
                                        .child("Applying settings for")
                                        .child(div().font_weight(FontWeight::MEDIUM).text_color(c.text).child("All projects"))
                                        .child("across")
                                        .child(div().font_weight(FontWeight::MEDIUM).text_color(c.text).child("All environments")),
                                )
                            })
                            .children(sections),
                    ),
                ),
            )
            .when_some(self.menu.as_ref(), |el, menu| el.child(menu.render()))
    }
}

impl SettingsView {
    fn general(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let c = cx.theme().colors.clone();
        let store = self.store.read(cx);
        let settings = store.config.as_ref().map(|c| c.settings.clone());
        let projects: Vec<_> = store.shell.projects_sorted().into_iter().cloned().collect();
        let thread_counts: Vec<usize> = projects
            .iter()
            .map(|p| store.shell.threads.iter().filter(|t| t.project_id == p.id).count())
            .collect();
        let server_version = store.config.as_ref().map(|c| c.environment.server_version.clone());
        let default_model = settings.as_ref().and_then(|s| s.default_model_selection.clone());
        // Without a default chosen, the web shows the model new threads get, "Automatic".
        let automatic = default_model.is_none();
        let effective = default_model
            .as_ref()
            .and_then(|m| serde_json::to_value(m).ok())
            .and_then(|v| crate::composer::ModelChoice::from_json(&v))
            .or_else(|| crate::composer::Composer::default_model(cx));
        let model_label: SharedString = effective
            .as_ref()
            .map(|m| {
                store
                    .provider(&m.instance_id)
                    .and_then(|p| p.models.iter().find(|x| x.slug == m.model))
                    .map(|x| x.short_name.clone().unwrap_or_else(|| x.name.clone()))
                    .unwrap_or_else(|| m.model.clone())
            })
            .unwrap_or_else(|| "Automatic".into())
            .into();
        let model_logo = effective.as_ref().map(|m| crate::ui::badges::provider_logo(&m.instance_id));
        let current_mode = settings
            .as_ref()
            .map(|s| s.default_runtime_mode.as_str().to_owned())
            .unwrap_or_else(|| "full-access".into());
        let on_merge = settings.as_ref().is_some_and(|s| s.sidebar_auto_settle_on_merge);
        let after_days = settings
            .as_ref()
            .and_then(|s| s.sidebar_auto_settle_after_days.as_ref())
            .and_then(|d| serde_json::to_value(d).ok())
            .and_then(|v| v.as_f64());
        let streaming = settings
            .as_ref()
            .map(|s| s.response_streaming_mode.as_str().to_owned())
            .unwrap_or_else(|| "paragraph".into());
        let streaming_label = match streaming.as_str() {
            "turn" => "Wait for the full response",
            "token" => "Token by token (legacy)",
            _ => "Show finished paragraphs",
        };
        let streaming_detail = match streaming.as_str() {
            "turn" => "Text appears once the agent finishes its turn.",
            "token" => "Every token repaints the answer as it arrives. Slower and harder to read. Thinking traces still arrive a paragraph at a time.",
            _ => "Each paragraph or code block appears as soon as it is complete.",
        };

        let model_control = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .h(px(28.))
            .px(px(10.))
            .rounded(px(radius::MD))
            .text_size(px(14.))
            .font_weight(FontWeight::MEDIUM)
            .text_color(c.text_2)
            .when_some(model_logo, |el, (glyph, color)| {
                el.child(svg().path(glyph.path()).size(px(16.)).text_color(color.unwrap_or(c.text_2)))
            })
            .child(model_label);
        let permissions = select(
            "default-permissions",
            Some(runtime_mode_icon(&current_mode)),
            runtime_mode_label(&current_mode),
            cx,
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                let options = RUNTIME_MODES.iter().map(|(m, l, _)| (json!(m), *l, Some(runtime_mode_icon(m)))).collect();
                let current = json!(this
                    .store
                    .read(cx)
                    .config
                    .as_ref()
                    .map(|c| c.settings.default_runtime_mode.as_str())
                    .unwrap_or("full-access"));
                this.open_select(event, options, current, "defaultRuntimeMode", window, cx);
            }),
        );
        let new_threads = vec![
            row(
                "Model",
                Some("Default model for new threads. Projects can override it.".into()),
                model_control.into_any_element(),
                cx,
            )
            .when(automatic, |el| {
                el.child(
                    div()
                        .absolute()
                        .left(px(16.))
                        .bottom(px(12.))
                        .pt(px(2.))
                        .text_size(px(12.))
                        .line_height(px(16.))
                        .text_color(c.text_2)
                        .child("Automatic"),
                )
                .relative()
                .pb(px(32.))
            })
            .into_any_element(),
            row(
                "Permissions",
                Some("Default permissions for new threads. Projects can override them.".into()),
                permissions.into_any_element(),
                cx,
            )
            .into_any_element(),
        ];
        let organization = vec![
            row(
                "Auto-settle merged threads",
                Some("Settle a thread when its pull request merges. Closed pull requests still settle automatically.".into()),
                switch("auto-settle-merged", on_merge, cx)
                    .on_click(cx.listener(move |this, _, _, cx| this.update_settings(json!({"sidebarAutoSettleOnMerge": !on_merge}), cx)))
                    .into_any_element(),
                cx,
            )
            .into_any_element(),
            row(
                "Auto-settle inactive threads",
                Some("Sidebar threads with no activity for this long settle automatically.".into()),
                switch("auto-settle-inactive", after_days.is_some(), cx)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let next = if after_days.is_some() { Value::Null } else { json!(3) };
                        this.update_settings(json!({"sidebarAutoSettleAfterDays": next}), cx)
                    }))
                    .into_any_element(),
                cx,
            )
            .into_any_element(),
            row(
                "Days of inactivity before auto-settle",
                Some("Any new activity un-settles a thread automatically.".into()),
                select("auto-settle-days", None, after_days.map(|d| format!("{d}")).unwrap_or_else(|| "Off".into()), cx)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            let options = vec![(json!(1), "1"), (json!(3), "3"), (json!(7), "7"), (json!(14), "14"), (json!(30), "30")]
                                .into_iter()
                                .map(|(v, l)| (v, l, None))
                                .collect();
                            this.open_select(
                                event,
                                options,
                                after_days.map(|d| json!(d as i64)).unwrap_or(Value::Null),
                                "sidebarAutoSettleAfterDays",
                                window,
                                cx,
                            );
                        }),
                    )
                    .into_any_element(),
                cx,
            )
            .into_any_element(),
        ];
        let behavior = vec![row(
            "Response streaming",
            Some(streaming_detail.into()),
            select("response-streaming", None, streaming_label, cx)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        let options = vec![
                            (json!("paragraph"), "Show finished paragraphs", None),
                            (json!("turn"), "Wait for the full response", None),
                            (json!("token"), "Token by token (legacy)", None),
                        ];
                        let current = json!(this
                            .store
                            .read(cx)
                            .config
                            .as_ref()
                            .map(|c| c.settings.response_streaming_mode.as_str())
                            .unwrap_or("paragraph"));
                        this.open_select(event, options, current, "responseStreamingMode", window, cx);
                    }),
                )
                .into_any_element(),
            cx,
        )
        .into_any_element()];

        // Projects.
        let projects_body = div()
            .flex()
            .flex_col()
            .children(projects.iter().zip(thread_counts).map(|(p, count)| {
                let id = p.id.as_str().to_owned();
                let title = p.title.clone();
                row(
                    p.title.clone(),
                    Some(format!("{} · {count} threads", p.workspace_root).into()),
                    Button::new(SharedString::from(format!("remove-{id}")))
                        .label("Remove…")
                        .small()
                        .variant(Variant::Danger)
                        .on_click(cx.listener(move |_this, _, window, cx| {
                            let answer = window.prompt(
                                PromptLevel::Warning,
                                &format!("Remove “{title}” from zenith?"),
                                Some("Its threads go too. The folder stays on disk."),
                                &["Remove Project", "Cancel"],
                                cx,
                            );
                            let id = id.clone();
                            cx.spawn(async move |this, cx| {
                                if answer.await == Ok(0) {
                                    let _ = this.update(cx, |this, cx| this.command("project.remove", json!({"projectId": id, "force": true}), cx));
                                }
                            })
                            .detach();
                        }))
                        .into_any_element(),
                    cx,
                )
            }))
            .child(
                div().px(px(16.)).py(px(10.)).child(
                    Button::new("settings-add-project")
                        .label("Add project…")
                        .icon(Icon::FolderPlus)
                        .small()
                        .variant(Variant::Secondary)
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(crate::actions::AddProject), cx)),
                ),
            )
            .into_any_element();

        // Updates.
        let update_state = update::updates(cx).read(cx).state.clone();
        let update_detail: SharedString = match &update_state {
            UpdateState::Idle => format!("zenith {}", zenith_commands::update::current_version()).into(),
            UpdateState::Checking => "Checking…".into(),
            UpdateState::Available(r) => format!("zenith {} is available", r.version).into(),
            UpdateState::Installing(r) => format!("Installing zenith {}…", r.version).into(),
            UpdateState::UpToDate => format!("zenith {} is the latest", zenith_commands::update::current_version()).into(),
            UpdateState::Failed(e) => e.clone().into(),
        };
        let workspace_for_toggle = self.workspace.clone();
        let check_updates = self.workspace.upgrade().map(|w| w.read(cx).prefs().check_for_updates).unwrap_or(true);
        let updates_body = div()
            .flex()
            .flex_col()
            .child(row(
                "Version",
                Some(update_detail),
                match update_state {
                    UpdateState::Available(_) => Button::new("install-update")
                        .label("Install and relaunch")
                        .icon(Icon::Download)
                        .small()
                        .variant(Variant::Primary)
                        .on_click(cx.listener(|_, _, _, cx| update::install(cx)))
                        .into_any_element(),
                    _ => Button::new("check-updates")
                        .label("Check now")
                        .small()
                        .variant(Variant::Secondary)
                        .on_click(|_, _, cx| update::check_now(cx))
                        .into_any_element(),
                },
                cx,
            ))
            .child(row(
                "Check when zenith starts",
                Some("From GitHub Releases; only releases signed by lsuite install.".into()),
                switch("check-on-start", check_updates, cx)
                    .on_click(move |_, _, cx| {
                        let _ = workspace_for_toggle.update(cx, |w, cx| w.set_check_for_updates(!check_updates, cx));
                    })
                    .into_any_element(),
                cx,
            ))
            .child(row(
                "Server",
                Some(
                    server_version
                        .map(|v| format!("zenith code server {v}"))
                        .unwrap_or_else(|| "Not connected".into())
                        .into(),
                ),
                Button::new("server-log")
                    .label("Show log")
                    .small()
                    .variant(Variant::Secondary)
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(crate::actions::ShowServerLog), cx))
                    .into_any_element(),
                cx,
            ))
            .into_any_element();

        vec![
            section("New threads", new_threads, cx),
            section("Organization", organization, cx),
            section("Behavior", behavior, cx),
            section("Projects", vec![projects_body], cx),
            section("Updates", vec![updates_body], cx),
        ]
    }

    fn appearance(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let appearance = self.workspace.upgrade().map(|w| w.read(cx).appearance()).unwrap_or_default();
        let label = match appearance {
            Appearance::System => "System",
            Appearance::Light => "Light",
            Appearance::Dark => "Dark",
        };
        let workspace = self.workspace.clone();
        let theme = select("theme", None, label, cx).on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                let workspace = workspace.clone();
                let entries = [(Appearance::System, "System"), (Appearance::Light, "Light"), (Appearance::Dark, "Dark")]
                    .into_iter()
                    .map(|(value, label)| {
                        let workspace = workspace.clone();
                        Entry::item(label, move |window, cx| {
                            let _ = workspace.update(cx, |w, cx| w.set_appearance(value, window, cx));
                        })
                        .checked(value == appearance)
                    })
                    .collect();
                this.menu = Some(OpenMenu::new(entries, event.position, window, cx, |this, _, _| this.menu = None));
                cx.notify();
            }),
        );
        vec![section(
            "Theme",
            vec![row("Mode", Some("Follow the system, or always light or dark.".into()), theme.into_any_element(), cx).into_any_element()],
            cx,
        )]
    }

    fn providers(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let c = cx.theme().colors.clone();
        let providers: Vec<_> = self.store.read(cx).providers().into_iter().cloned().collect();
        // Providers.
        let providers_body = div()
            .flex()
            .flex_col()
            .children(providers.iter().map(|p| {
                let status = p.status.as_str();
                let color = match status {
                    "ready" => c.success,
                    "warning" => c.warning,
                    "error" => c.danger,
                    _ => c.text_3,
                };
                let mut detail = Vec::new();
                if let Some(version) = &p.version {
                    detail.push(format!("v{version}"));
                }
                if let Some(label) = p.auth.label.clone().or(p.auth.email.clone()) {
                    detail.push(label);
                }
                detail.push(format!("{} models", p.models.len()));
                if let Some(message) = &p.message {
                    detail.push(message.clone());
                }
                row(
                    provider_name(p),
                    Some(detail.join(" · ").into()),
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(dot(color))
                        .child(div().text_size(px(text::SM)).text_color(c.text_2).child(SharedString::from(if p.enabled {
                            status.to_owned()
                        } else {
                            "disabled".into()
                        })))
                        .into_any_element(),
                    cx,
                )
            }))
            .child(
                div().px(px(16.)).py(px(10.)).child(
                    Button::new("refresh-providers")
                        .label("Check again")
                        .icon(Icon::Refresh)
                        .small()
                        .variant(Variant::Secondary)
                        .on_click(cx.listener(|this, _, _, cx| this.command("provider.refresh", json!({}), cx))),
                ),
            )
            .into_any_element();

        vec![section("Providers", vec![providers_body], cx)]
    }

    fn integrations(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let c = cx.theme().colors.clone();
        let this = cx.entity().downgrade();
        let access = self.permissions.mcp;
        let mcp_path = zenith_commands::lsuite::sibling("zenith-mcp")
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "zenith-mcp".into());
        let mcp_command = format!("claude mcp add zenith -- {mcp_path} --live");
        let access_label = match access {
            Access::Off => "Off",
            Access::Read => "Read only",
            Access::Full => "Full",
        };
        let access_select = select("mcp-access", None, access_label, cx).on_mouse_down(
            MouseButton::Left,
            cx.listener(move |view, event: &MouseDownEvent, window, cx| {
                let entries = [(Access::Off, "Off"), (Access::Read, "Read only"), (Access::Full, "Full")]
                    .into_iter()
                    .map(|(value, label)| {
                        let this = this.clone();
                        Entry::item(label, move |_, cx| {
                            let _ = this.update(cx, |s, cx| s.set_permissions(value, cx));
                        })
                        .checked(value == access)
                    })
                    .collect();
                view.menu = Some(OpenMenu::new(entries, event.position, window, cx, |this, _, _| this.menu = None));
                cx.notify();
            }),
        );
        let copy_command = mcp_command.clone();
        let agents = vec![
            row(
                "Agents driving zenith",
                Some("What an agent connected with zenith-mcp may do: nothing, read, or everything (start threads, answer approvals…).".into()),
                access_select.into_any_element(),
                cx,
            )
            .into_any_element(),
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(16.))
                .py(px(12.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .one_line()
                        .font_family(MONO_FONT)
                        .text_size(px(12.))
                        .text_color(c.text_2)
                        .child(SharedString::from(mcp_command)),
                )
                .child(
                    Button::new("copy-mcp")
                        .icon(Icon::Copy)
                        .label("Copy")
                        .small()
                        .variant(Variant::Secondary)
                        .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy_command.clone()))),
                )
                .into_any_element(),
        ];
        // lsuite.
        let apps = zenith_commands::lsuite::installed_apps();
        let lsuite_body: AnyElement = if apps.is_empty() {
            div()
                .p(px(16.))
                .text_size(px(text::BASE))
                .text_color(c.text_3)
                .child("No other lsuite app on this Mac yet. The suite's music and video apps show here once installed; their MCP servers are then offered to the agents of your threads.")
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .children(apps.iter().map(|app| {
                    let mcp = app.mcp.as_ref().map(|m| m.server());
                    let offered = mcp.as_ref().is_some_and(|m| std::path::Path::new(&m.command).is_file());
                    let kind = app.kind.clone().unwrap_or_else(|| "lsuite app".into());
                    row(
                        format!("{} {}", app.app, app.version),
                        Some(match &mcp {
                            Some(mcp) => format!("{kind} · MCP: {}", mcp.command).into(),
                            None => format!("{kind} · no MCP server").into(),
                        }),
                        if offered {
                            pill("Offered to agents", c.success, gpui::Hsla { a: 0.14, ..c.success })
                        } else {
                            pill("Not offered", c.text_3, c.hover)
                        }
                        .into_any_element(),
                        cx,
                    )
                }))
                .into_any_element()
        };

        vec![section("Agents", agents, cx), section("lsuite apps", vec![lsuite_body], cx)]
    }

    /// The server this window drives: this machine's, or one paired with `zenith-cli remote`.
    fn connections(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let c = cx.theme().colors.clone();
        let store = self.store.read(cx);
        let remote = store.is_remote();
        let connected = store.connected();
        let url = store.client.base_url().to_owned();
        let machine = store.server_name();
        let version = store.config.as_ref().map(|c| c.environment.server_version.clone());
        let status = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .child(dot(if connected { c.success } else { c.warning }))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(c.text_2)
                    .child(if connected { "Connected" } else { "Offline" }),
            );
        let mut rows = vec![
            row(
                SharedString::from(machine.to_string()),
                Some(
                    format!(
                        "{}{} · {url}",
                        if remote { "A server on another machine" } else { "This machine's server" },
                        version.map(|v| format!(", zenith code {v}")).unwrap_or_default()
                    )
                    .into(),
                ),
                status.into_any_element(),
                cx,
            )
            .into_any_element(),
            row(
                if remote {
                    "Back to this machine's server"
                } else {
                    "Drive a server on another machine"
                },
                Some(
                    if remote {
                        "In a terminal: zenith-cli remote --off, then reopen zenith."
                    } else {
                        "On that server: zenith-code auth pairing create --admin; here: zenith-cli remote <its https address> <code>, then reopen zenith."
                    }
                    .into(),
                ),
                div().into_any_element(),
                cx,
            )
            .into_any_element(),
        ];
        if !remote {
            rows.push(
                row(
                    "Server log",
                    Some(zenith_client::local::server_log().display().to_string().into()),
                    Button::new("server-log")
                        .label("Show log")
                        .small()
                        .variant(Variant::Secondary)
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(crate::actions::ShowServerLog), cx))
                        .into_any_element(),
                    cx,
                )
                .into_any_element(),
            );
        }
        vec![section("Server", rows, cx)]
    }

    fn archive(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let c = cx.theme().colors.clone();
        // Archived threads.
        let archived_body: AnyElement = match &self.archived {
            None => div().p(px(16.)).text_color(c.text_3).child("Loading…").into_any_element(),
            Some(threads) if threads.is_empty() => div()
                .p(px(16.))
                .text_size(px(text::BASE))
                .text_color(c.text_3)
                .child("Nothing archived.")
                .into_any_element(),
            Some(threads) => div()
                .flex()
                .flex_col()
                .children(threads.iter().take(50).map(|t| {
                    let id = t.id.as_str().to_owned();
                    let id2 = id.clone();
                    row(
                        t.title.clone(),
                        t.archived_at.clone().map(|a| SharedString::from(format!("Archived {}", &a[..10.min(a.len())]))),
                        div()
                            .flex()
                            .gap(px(6.))
                            .child(
                                Button::new(SharedString::from(format!("unarchive-{id}")))
                                    .label("Restore")
                                    .icon(Icon::ArchiveRestore)
                                    .small()
                                    .variant(Variant::Secondary)
                                    .on_click(cx.listener(move |this, _, _, cx| this.command("thread.unarchive", json!({"threadId": id}), cx))),
                            )
                            .child(
                                Button::new(SharedString::from(format!("delete-{id2}")))
                                    .icon(Icon::Trash)
                                    .small()
                                    .variant(Variant::Danger)
                                    .tooltip("Delete for good")
                                    .on_click(cx.listener(move |this, _, _, cx| this.command("thread.delete", json!({"threadId": id2}), cx))),
                            )
                            .into_any_element(),
                        cx,
                    )
                }))
                .into_any_element(),
        };

        vec![section("Archived threads", vec![archived_body], cx)]
    }
}
