//! Settings: appearance, how agents start (approval mode, model), what agents may do through
//! zenith-mcp, the providers, the projects, archived threads, the other lsuite apps, updates.
//! Every change goes through the command registry, like the CLI's.

use gpui::prelude::*;
use gpui::{div, px, AnyElement, App, ClipboardItem, Context, Entity, FontWeight, PromptLevel, SharedString, Subscription, WeakEntity, Window};
use serde_json::{json, Value};
use zc_contracts::OrchestrationThreadShell;
use zenith_commands::permissions::{Access, Permissions};

use crate::assets::{Icon, MONO_FONT};
use crate::composer::{provider_name, runtime_mode_label, RUNTIME_MODES};
use crate::store::{self, Store};
use crate::theme::{radius, text, ActiveTheme, Appearance};
use crate::ui::{caps_label, dot, icon, pill, Button, Variant};
use crate::update::{self, UpdateState};
use crate::workspace::Workspace;

pub struct SettingsView {
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

fn section(title: &str, detail: Option<&str>, body: AnyElement, cx: &App) -> AnyElement {
    let c = cx.theme().colors.clone();
    div()
        .flex()
        .flex_col()
        .gap(px(12.))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .text_size(px(text::LG))
                        .font_weight(FontWeight::BOLD)
                        .text_color(c.text)
                        .child(SharedString::from(title.to_owned())),
                )
                .when_some(detail, |this, detail| {
                    this.child(
                        div()
                            .text_size(px(text::BASE))
                            .text_color(c.text_2)
                            .child(SharedString::from(detail.to_owned())),
                    )
                }),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .rounded(px(radius::LG))
                .border_1()
                .border_color(c.line)
                .bg(c.bg_sunken)
                .child(body),
        )
        .into_any_element()
}

fn row(label: impl Into<SharedString>, detail: Option<SharedString>, right: AnyElement, cx: &App) -> gpui::Div {
    let c = cx.theme().colors.clone();
    div()
        .flex()
        .items_center()
        .gap(px(12.))
        .px(px(16.))
        .py(px(12.))
        .border_b_1()
        .border_color(c.line)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .text_size(px(text::BASE))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(c.text)
                        .child(label.into()),
                )
                .when_some(detail, |this, detail| {
                    this.child(div().text_size(px(text::SM)).text_color(c.text_3).child(detail))
                }),
        )
        .child(right)
}

fn segmented<T: Copy + PartialEq + 'static>(
    id: &'static str,
    options: &[(T, &'static str)],
    current: T,
    on_pick: impl Fn(T, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let c = cx.theme().colors.clone();
    let on_pick = std::rc::Rc::new(on_pick);
    div()
        .flex()
        .p(px(2.))
        .gap(px(2.))
        .rounded(px(radius::SM))
        .bg(c.hover)
        .children(options.iter().enumerate().map(|(i, (value, label))| {
            let value = *value;
            let on_pick = on_pick.clone();
            Button::new(SharedString::from(format!("{id}-{i}")))
                .label(*label)
                .small()
                .selected(value == current)
                .on_click(move |_, window, cx| on_pick(value, window, cx))
        }))
        .into_any_element()
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = cx.theme().colors.clone();
        let appearance = self.workspace.upgrade().map(|w| w.read(cx).appearance()).unwrap_or_default();
        let check_updates = self.workspace.upgrade().map(|w| w.read(cx).prefs().check_for_updates).unwrap_or(true);
        let store = self.store.read(cx);
        let settings = store.config.as_ref().map(|c| c.settings.clone());
        let providers: Vec<_> = store.providers().into_iter().cloned().collect();
        let projects: Vec<_> = store.shell.projects_sorted().into_iter().cloned().collect();
        let thread_counts: Vec<usize> = projects
            .iter()
            .map(|p| store.shell.threads.iter().filter(|t| t.project_id == p.id).count())
            .collect();
        let server_version = store.config.as_ref().map(|c| c.environment.server_version.clone());
        let workspace = self.workspace.clone();

        // Appearance.
        let appearance_body = row(
            "Appearance",
            Some("zenith follows macOS unless you choose.".into()),
            segmented(
                "appearance",
                &[(Appearance::System, "System"), (Appearance::Light, "Light"), (Appearance::Dark, "Dark")],
                appearance,
                move |value, window, cx| {
                    let _ = workspace.update(cx, |w, cx| w.set_appearance(value, window, cx));
                },
                cx,
            ),
            cx,
        )
        .into_any_element();

        // How agents start.
        let current_mode = settings
            .as_ref()
            .map(|s| s.default_runtime_mode.as_str().to_owned())
            .unwrap_or_else(|| "full-access".into());
        let this = cx.entity().downgrade();
        let modes: Vec<(&'static str, &'static str)> = RUNTIME_MODES.iter().map(|(m, l, _)| (*m, *l)).collect();
        let mode_picker = {
            let this = this.clone();
            segmented(
                "runtime-mode",
                &modes,
                RUNTIME_MODES.iter().map(|(m, _, _)| *m).find(|m| *m == current_mode).unwrap_or("full-access"),
                move |value, _, cx| {
                    let _ = this.update(cx, |s, cx| s.command("settings.update", json!({"patch": {"defaultRuntimeMode": value}}), cx));
                },
                cx,
            )
        };
        let default_model = settings
            .as_ref()
            .and_then(|s| s.default_model_selection.as_ref())
            .map(|m| format!("{} · {}", m.instance_id.as_str(), m.model))
            .unwrap_or_else(|| "The first ready provider's default".into());
        let access = self.permissions.mcp;
        let mcp_path = zenith_commands::lsuite::sibling("zenith-mcp")
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "zenith-mcp".into());
        let mcp_command = format!("claude mcp add zenith -- {mcp_path} --live");
        let permissions_picker = {
            let this = this.clone();
            segmented(
                "mcp-access",
                &[(Access::Off, "Off"), (Access::Read, "Read only"), (Access::Full, "Full")],
                access,
                move |value, _, cx| {
                    let _ = this.update(cx, |s, cx| s.set_permissions(value, cx));
                },
                cx,
            )
        };
        let copy_command = mcp_command.clone();
        let agents_body = div()
            .flex()
            .flex_col()
            .child(row(
                "New threads ask",
                Some(format!("Now: {}", runtime_mode_label(&current_mode)).into()),
                mode_picker,
                cx,
            ))
            .child(row("Default model", Some(default_model.into()), div().into_any_element(), cx))
            .child(row(
                "Agents driving zenith",
                Some("What an agent connected with zenith-mcp may do: nothing, read, or everything (start threads, answer approvals…).".into()),
                permissions_picker,
                cx,
            ))
            .child(
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
                            .truncate()
                            .font_family(MONO_FONT)
                            .text_size(px(text::SM))
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
                    ),
            )
            .into_any_element();

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
                segmented(
                    "check-on-start",
                    &[(true, "On"), (false, "Off")],
                    check_updates,
                    move |value, _, cx| {
                        let _ = workspace_for_toggle.update(cx, |w, cx| w.set_check_for_updates(value, cx));
                    },
                    cx,
                ),
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

        let about = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .text_size(px(text::SM))
            .text_color(c.text_3)
            .child(icon(Icon::Sparkles, c.accent_text))
            .child("zenith is part of lsuite: free, open source (MIT), no account, no telemetry.")
            .child(
                Button::new("lsuite-page")
                    .label("lsuite.xyz/zenith")
                    .small()
                    .on_click(|_, _, cx| cx.open_url("https://lsuite.xyz/zenith")),
            )
            .child(
                Button::new("support")
                    .label("Support")
                    .icon(Icon::ExternalLink)
                    .small()
                    .on_click(|_, _, cx| cx.open_url("https://lsuite.xyz/zenith/support")),
            );

        div().size_full().child(
            div().id("settings-scroll").size_full().overflow_y_scroll().child(
                div().flex().justify_center().px(px(32.)).py(px(28.)).child(
                    div()
                        .w_full()
                        .max_w(px(760.))
                        .flex()
                        .flex_col()
                        .gap(px(32.))
                        .child(section("Appearance", None, appearance_body, cx))
                        .child(section(
                            "Agents",
                            Some("How new threads start, and what agents outside zenith may do with it."),
                            agents_body,
                            cx,
                        ))
                        .child(section(
                            "Providers",
                            Some("The coding agents zenith runs. Sign in with each agent's own CLI."),
                            providers_body,
                            cx,
                        ))
                        .child(section("Projects", None, projects_body, cx))
                        .child(section("Archived threads", None, archived_body, cx))
                        .child(section("lsuite apps", Some("Other lsuite apps on this Mac (~/.lsuite/apps)."), lsuite_body, cx))
                        .child(section("Updates", None, updates_body, cx))
                        .child(caps_label("About", cx))
                        .child(about),
                ),
            ),
        )
    }
}
