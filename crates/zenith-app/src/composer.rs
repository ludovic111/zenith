//! The composer (glass, under the thread): the message, then the model (provider instance
//! and model, with its options such as reasoning effort), the approval mode, Plan or Build,
//! where a new thread works (this checkout or a new worktree), and Send or Stop.

use gpui::prelude::*;
use gpui::{div, px, App, Context, Entity, EventEmitter, FontWeight, MouseButton, MouseDownEvent, SharedString, Subscription, Window};
use serde_json::Value;
use zc_contracts::ServerProvider;

use crate::assets::Icon;
use crate::store::{self, Store};
use crate::theme::{radius, text, ActiveTheme};
use crate::ui::menu::{Entry, OpenMenu};
use crate::ui::text_area::{TextArea, TextAreaEvent};
use crate::ui::{icon, Button, Variant};

pub enum ComposerEvent {
    Send(String),
    Stop,
}

/// What the agent runs with.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelChoice {
    pub instance_id: String,
    pub model: String,
    /// `[{id, value}]` (reasoning effort, fast mode…).
    pub options: Vec<(String, Value)>,
}

impl ModelChoice {
    pub fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            instance_id: value.get("instanceId")?.as_str()?.to_owned(),
            model: value.get("model")?.as_str()?.to_owned(),
            options: value
                .get("options")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|o| Some((o.get("id")?.as_str()?.to_owned(), o.get("value")?.clone())))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
}

/// The four approval modes, as the web interface names them.
pub const RUNTIME_MODES: &[(&str, &str, &str)] = &[
    ("approval-required", "Supervised", "Asks before running commands and changing files"),
    ("auto-accept-edits", "Auto-accept edits", "Changes files freely, asks before commands"),
    ("auto", "Auto", "Decides what needs your approval"),
    ("full-access", "Full access", "Never asks"),
];

pub fn runtime_mode_label(mode: &str) -> &'static str {
    RUNTIME_MODES.iter().find(|(m, _, _)| *m == mode).map(|(_, l, _)| *l).unwrap_or("Full access")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvMode {
    Local,
    Worktree,
}

pub struct Composer {
    store: Entity<Store>,
    pub editor: Entity<TextArea>,
    pub model: Option<ModelChoice>,
    pub runtime_mode: String,
    pub plan_mode: bool,
    /// Only for a thread not created yet.
    pub env_mode: Option<EnvMode>,
    pub running: bool,
    pub disabled_reason: Option<SharedString>,
    /// Images to send with the next message (files; pasted ones are saved first).
    pub images: Vec<std::path::PathBuf>,
    menu: Option<OpenMenu>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ComposerEvent> for Composer {}

pub fn provider_name(provider: &ServerProvider) -> String {
    provider.display_name.clone().unwrap_or_else(|| match provider.driver.as_str() {
        "claudeAgent" => "Claude Code".into(),
        "codex" => "Codex".into(),
        other => other.to_owned(),
    })
}

impl Composer {
    pub fn new(placeholder: &str, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = store::store(cx);
        let editor = cx.new(|cx| TextArea::multi_line(1, 14, cx).with_placeholder(placeholder.to_owned()));
        let subscriptions = vec![
            cx.subscribe_in(&editor, window, |this, _, event: &TextAreaEvent, _, cx| match event {
                TextAreaEvent::Submit => this.send(cx),
                TextAreaEvent::Changed => cx.notify(),
                TextAreaEvent::PastedImage { extension, bytes } => this.keep_pasted_image(extension, bytes, cx),
                _ => {}
            }),
            cx.observe(&store, |this, _, cx| {
                // A draft opened before the server's configuration arrived gets its default model.
                if this.model.is_none() {
                    this.model = Composer::default_model(cx);
                }
                cx.notify()
            }),
        ];
        Self {
            store,
            editor,
            model: None,
            runtime_mode: "full-access".into(),
            plan_mode: false,
            env_mode: None,
            running: false,
            disabled_reason: None,
            images: Vec::new(),
            menu: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &App) {
        self.editor.read(cx).focus(window);
    }

    /// The model to use when nothing chose one: the server's default, else the first ready
    /// provider's default model.
    pub fn default_model(cx: &App) -> Option<ModelChoice> {
        let store = store::store(cx);
        let store = store.read(cx);
        let config = store.config.as_ref()?;
        if let Some(selection) = config.settings.default_model_selection.as_ref() {
            if let Some(choice) = serde_json::to_value(selection).ok().and_then(|v| ModelChoice::from_json(&v)) {
                return Some(choice);
            }
        }
        let provider = store
            .providers()
            .into_iter()
            .find(|p| p.enabled && p.installed && !p.models.is_empty() && p.status.as_str() != "error")?;
        let model = provider
            .models
            .iter()
            .find(|m| m.is_default == Some(true))
            .or_else(|| provider.models.first())?;
        Some(ModelChoice {
            instance_id: provider.instance_id.as_str().to_owned(),
            model: model.slug.clone(),
            options: Vec::new(),
        })
    }

    fn send(&mut self, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text().trim().to_owned();
        if text.is_empty() || self.running || self.disabled_reason.is_some() {
            return;
        }
        cx.emit(ComposerEvent::Send(text));
    }

    /// After a send went through.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.editor.update(cx, |e, cx| e.clear(cx));
        self.images.clear();
        cx.notify();
    }

    /// Saves a pasted image under `~/.zenith/app/attachments` and attaches it.
    fn keep_pasted_image(&mut self, extension: &str, bytes: &[u8], cx: &mut Context<Self>) {
        let dir = zenith_client::local::app_home().join("attachments");
        let path = dir.join(format!("pasted-{}.{extension}", zenith_client::new_id()));
        match std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, bytes)) {
            Ok(()) => {
                self.images.push(path);
                cx.notify();
            }
            Err(error) => {
                let message = format!("Could not keep the pasted image: {error}");
                self.store.update(cx, |s, cx| s.notify_error(message, cx));
            }
        }
    }

    fn attach_images(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let _ = this.update(cx, |this, cx| {
                for path in paths {
                    let image = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp"));
                    if image {
                        this.images.push(path);
                    } else {
                        let message = format!("{} is not a PNG, JPEG, GIF or WebP image", path.display());
                        this.store.update(cx, |s, cx| s.notify_error(message, cx));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn model_label(&self, cx: &App) -> SharedString {
        let Some(model) = &self.model else {
            return "Choose a model".into();
        };
        let store = self.store.read(cx);
        let name = store
            .provider(&model.instance_id)
            .and_then(|p| p.models.iter().find(|m| m.slug == model.model))
            .map(|m| m.short_name.clone().unwrap_or_else(|| m.name.clone()))
            .unwrap_or_else(|| model.model.clone());
        let effort = model
            .options
            .iter()
            .find(|(id, _)| id.contains("effort") || id.contains("reasoning"))
            .and_then(|(_, v)| v.as_str().map(String::from));
        match effort {
            Some(effort) => format!("{name} · {effort}").into(),
            None => name.into(),
        }
    }

    fn open_model_menu(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let mut entries = Vec::new();
        let providers: Vec<ServerProvider> = self.store.read(cx).providers().into_iter().cloned().collect();
        for provider in providers {
            if !provider.enabled || provider.models.is_empty() {
                continue;
            }
            entries.push(Entry::Header(provider_name(&provider).into()));
            for model in provider.models.iter().filter(|m| m.is_legacy != Some(true)) {
                let choice = ModelChoice {
                    instance_id: provider.instance_id.as_str().to_owned(),
                    model: model.slug.clone(),
                    options: Vec::new(),
                };
                let checked = self
                    .model
                    .as_ref()
                    .is_some_and(|m| m.instance_id == choice.instance_id && m.model == choice.model);
                let this = this.clone();
                entries.push(
                    Entry::item(model.name.clone(), move |_, cx| {
                        let _ = this.update(cx, |c, cx| {
                            c.model = Some(choice.clone());
                            cx.notify();
                        });
                    })
                    .checked(checked),
                );
            }
        }
        // The chosen model's options.
        if let Some(current) = self.model.clone() {
            let store = self.store.read(cx);
            let descriptors = store
                .provider(&current.instance_id)
                .and_then(|p| p.models.iter().find(|m| m.slug == current.model))
                .and_then(|m| m.capabilities.as_ref())
                .and_then(|c| serde_json::to_value(c).ok())
                .and_then(|v| v.get("optionDescriptors").cloned())
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default();
            for descriptor in descriptors {
                let Some(id) = descriptor.get("id").and_then(Value::as_str).map(String::from) else {
                    continue;
                };
                let label = descriptor.get("label").and_then(Value::as_str).unwrap_or(&id).to_owned();
                let selected = current
                    .options
                    .iter()
                    .find(|(o, _)| *o == id)
                    .map(|(_, v)| v.clone())
                    .or_else(|| descriptor.get("currentValue").cloned());
                entries.push(Entry::Separator);
                entries.push(Entry::Header(label.clone().into()));
                match descriptor.get("type").and_then(Value::as_str) {
                    Some("select") => {
                        for option in descriptor.get("options").and_then(Value::as_array).cloned().unwrap_or_default() {
                            let Some(value) = option.get("id").and_then(Value::as_str).map(String::from) else {
                                continue;
                            };
                            let option_label = option.get("label").and_then(Value::as_str).unwrap_or(&value).to_owned();
                            let is_default = option.get("isDefault").and_then(Value::as_bool) == Some(true);
                            let checked = selected.as_ref().and_then(Value::as_str).map(|s| s == value).unwrap_or(is_default);
                            let this = this.clone();
                            let id = id.clone();
                            entries.push(
                                Entry::item(option_label, move |_, cx| {
                                    let _ = this.update(cx, |c, cx| {
                                        if let Some(model) = c.model.as_mut() {
                                            model.options.retain(|(o, _)| *o != id);
                                            model.options.push((id.clone(), Value::String(value.clone())));
                                        }
                                        cx.notify();
                                    });
                                })
                                .checked(checked),
                            );
                        }
                    }
                    Some("boolean") => {
                        let on = selected.as_ref().and_then(Value::as_bool).unwrap_or(false);
                        let this = this.clone();
                        entries.push(
                            Entry::item(if on { "On" } else { "Off" }, move |_, cx| {
                                let _ = this.update(cx, |c, cx| {
                                    if let Some(model) = c.model.as_mut() {
                                        model.options.retain(|(o, _)| *o != id);
                                        model.options.push((id.clone(), Value::Bool(!on)));
                                    }
                                    cx.notify();
                                });
                            })
                            .detail("click to switch")
                            .checked(on),
                        );
                    }
                    _ => {}
                }
            }
        }
        if entries.is_empty() {
            entries.push(Entry::item("No provider is ready (see Settings)", |_, _| {}).disabled(true));
        }
        self.menu = Some(OpenMenu::new(entries, event.position, window, cx, |this, _, _| this.menu = None).upward());
        cx.notify();
    }

    fn open_mode_menu(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let entries = RUNTIME_MODES
            .iter()
            .map(|(mode, label, detail)| {
                let this = this.clone();
                let mode = mode.to_string();
                let checked = self.runtime_mode == mode;
                Entry::item(*label, move |_, cx| {
                    let _ = this.update(cx, |c, cx| {
                        c.runtime_mode = mode.clone();
                        cx.notify();
                    });
                })
                .detail(*detail)
                .checked(checked)
            })
            .collect();
        self.menu = Some(OpenMenu::new(entries, event.position, window, cx, |this, _, _| this.menu = None).upward());
        cx.notify();
    }

    fn open_env_menu(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let entries = [
            (EnvMode::Local, "This checkout", "Works in the project's folder"),
            (EnvMode::Worktree, "New worktree", "Works on a new branch in its own folder"),
        ]
        .into_iter()
        .map(|(mode, label, detail)| {
            let this = this.clone();
            Entry::item(label, move |_, cx| {
                let _ = this.update(cx, |c, cx| {
                    c.env_mode = Some(mode);
                    cx.notify();
                });
            })
            .detail(detail)
            .checked(self.env_mode == Some(mode))
        })
        .collect();
        self.menu = Some(OpenMenu::new(entries, event.position, window, cx, |this, _, _| this.menu = None).upward());
        cx.notify();
    }
}

fn chip(id: &'static str, icon_name: Icon, label: SharedString, cx: &App) -> gpui::Stateful<gpui::Div> {
    let c = &cx.theme().colors;
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(6.))
        .h(px(26.))
        .px(px(8.))
        .rounded(px(radius::SM))
        .cursor_pointer()
        .text_size(px(text::SM))
        .font_weight(FontWeight::MEDIUM)
        .text_color(c.text_2)
        .hover(|s| s.bg(c.hover))
        .child(icon(icon_name, c.text_3).size(px(13.)))
        .child(div().max_w(px(220.)).truncate().child(label))
        .child(icon(Icon::ChevronDown, c.text_3).size(px(11.)))
}

impl Render for Composer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let c = &theme.colors;
        let empty = self.editor.read(cx).text().trim().is_empty();
        let model_label = self.model_label(cx);
        let plan = self.plan_mode;
        div()
            .flex()
            .flex_col()
            .w_full()
            .rounded(px(radius::LG))
            .bg(c.glass_2)
            .border_1()
            .border_color(c.glass_edge)
            .shadow(theme.floating_shadow())
            .when(!self.images.is_empty(), |this| {
                this.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(8.))
                        .px(px(16.))
                        .pt(px(12.))
                        .children(self.images.iter().enumerate().map(|(i, path)| {
                            div()
                                .relative()
                                .size(px(56.))
                                .rounded(px(radius::SM))
                                .border_1()
                                .border_color(c.line_strong)
                                .overflow_hidden()
                                .child(gpui::img(path.clone()).size_full().object_fit(gpui::ObjectFit::Cover))
                                .child(
                                    div().absolute().top(px(2.)).right(px(2.)).child(
                                        Button::new(SharedString::from(format!("unattach-{i}")))
                                            .icon(Icon::X)
                                            .small()
                                            .variant(Variant::Secondary)
                                            .tooltip("Remove")
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                if i < this.images.len() {
                                                    this.images.remove(i);
                                                }
                                                cx.notify();
                                            })),
                                    ),
                                )
                        })),
                )
            })
            .child(div().px(px(16.)).pt(px(14.)).pb(px(6.)).child(self.editor.clone()))
            .when_some(self.disabled_reason.clone(), |this, reason| {
                this.child(div().px(px(16.)).pb(px(4.)).text_size(px(text::SM)).text_color(c.warning).child(reason))
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(2.))
                    .px(px(8.))
                    .pb(px(8.))
                    .child(
                        Button::new("attach")
                            .icon(Icon::Paperclip)
                            .tooltip("Attach images (or paste one)")
                            .on_click(cx.listener(|this, _, _, cx| this.attach_images(cx))),
                    )
                    .child(chip("model", Icon::Cpu, model_label, cx).on_mouse_down(MouseButton::Left, cx.listener(Self::open_model_menu)))
                    .child(
                        chip("runtime-mode", Icon::Shield, runtime_mode_label(&self.runtime_mode).into(), cx)
                            .on_mouse_down(MouseButton::Left, cx.listener(Self::open_mode_menu)),
                    )
                    .when_some(self.env_mode, |this, env| {
                        this.child(
                            chip(
                                "env-mode",
                                Icon::GitBranch,
                                match env {
                                    EnvMode::Local => "This checkout".into(),
                                    EnvMode::Worktree => "New worktree".into(),
                                },
                                cx,
                            )
                            .on_mouse_down(MouseButton::Left, cx.listener(Self::open_env_menu)),
                        )
                    })
                    .child(
                        Button::new("plan-mode")
                            .label(if plan { "Plan" } else { "Build" })
                            .icon(if plan { Icon::ListChecks } else { Icon::Hammer })
                            .small()
                            .selected(plan)
                            .tooltip("Plan first: the agent proposes a plan and waits")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.plan_mode = !this.plan_mode;
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1())
                    .child(if self.running {
                        Button::new("stop")
                            .icon(Icon::CircleStop)
                            .label("Stop")
                            .variant(Variant::Secondary)
                            .tooltip_keys("Stop the agent", "⌘.")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ComposerEvent::Stop)))
                            .into_any_element()
                    } else {
                        Button::new("send")
                            .icon(Icon::ArrowUp)
                            .variant(Variant::Primary)
                            .disabled(empty || self.disabled_reason.is_some())
                            .tooltip_keys("Send", "↩")
                            .on_click(cx.listener(|this, _, _, cx| this.send(cx)))
                            .into_any_element()
                    }),
            )
            .when_some(self.menu.as_ref(), |this, menu| this.child(menu.render()))
    }
}
