//! The composer, as the web's (`ChatComposer`): the message, then the model, its options
//! ("Medium · 1M"), the approval mode (with Build or Plan), the paperclip and Send or Stop;
//! under it, a strip with where the thread works, its pull request and its branch; over it,
//! "Monitoring" while the agent's background work watches.

use gpui::prelude::*;
use gpui::{div, px, svg, App, BoxShadow, Context, Entity, EventEmitter, FontWeight, MouseButton, MouseDownEvent, SharedString, Subscription, Window};
use serde_json::Value;
use zc_contracts::ServerProvider;

use crate::assets::Icon;
use crate::store::{self, Store};
use crate::theme::{radius, ActiveTheme};
use crate::ui::menu::{Entry, OpenMenu};
use crate::ui::text_area::{TextArea, TextAreaEvent};
use crate::ui::OneLine;

pub enum ComposerEvent {
    Send(String),
    Stop,
    /// The banner's action: `thread.reopen` (settled), `thread.wake` (snoozed).
    Command(&'static str),
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

/// The four approval modes, as the web interface names and describes them
/// (`runtimeModeConfig.ts`).
pub const RUNTIME_MODES: &[(&str, &str, &str)] = &[
    ("approval-required", "Supervised", "Ask before commands and file changes."),
    ("auto-accept-edits", "Auto-accept edits", "Auto-approve edits, ask before other actions."),
    ("auto", "Auto", "Supported providers approve routine actions; others still ask."),
    ("full-access", "Full access", "Allow commands and edits without prompts."),
];

/// A mode's icon.
pub fn runtime_mode_icon(mode: &str) -> Icon {
    match mode {
        "approval-required" => Icon::Lock,
        "auto-accept-edits" => Icon::PenLine,
        "auto" => Icon::Sparkles,
        _ => Icon::LockOpen,
    }
}

pub fn runtime_mode_label(mode: &str) -> &'static str {
    RUNTIME_MODES.iter().find(|(m, _, _)| *m == mode).map(|(_, l, _)| *l).unwrap_or("Full access")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvMode {
    Local,
    Worktree,
}

/// What the composer shows around the message: the thread's workspace and branch in the strip
/// under it, its pull request, and the "Monitoring" banner over it.
#[derive(Clone, Debug, Default)]
pub struct ComposerContext {
    /// A thread that is not created yet: its workspace can still be chosen.
    pub draft: bool,
    pub branch: Option<String>,
    /// It works in a worktree of its own.
    pub worktree: bool,
    pub pull_request: Option<zenith_model::shell::PullRequestBadge>,
    /// The agent's background work watches (`backgroundLiveness`).
    pub monitoring: bool,
    pub settled: bool,
    pub snoozed: bool,
}

pub struct Composer {
    store: Entity<Store>,
    pub context: ComposerContext,
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
        // 14 on 22.75, at least 70 px, scrolling past about 200 (`composer-tiptap`).
        let editor = cx.new(|cx| TextArea::multi_line(3, 8, cx).with_placeholder(placeholder.to_owned()).with_font(14., 22.75));
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
            context: ComposerContext::default(),
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
        store
            .provider(&model.instance_id)
            .and_then(|p| p.models.iter().find(|m| m.slug == model.model))
            .map(|m| m.short_name.clone().unwrap_or_else(|| m.name.clone()))
            .unwrap_or_else(|| model.model.clone())
            .into()
    }

    /// The model's option descriptors (reasoning effort, context window, fast mode…).
    fn descriptors(&self, cx: &App) -> Vec<Value> {
        let Some(current) = &self.model else { return Vec::new() };
        self.store
            .read(cx)
            .provider(&current.instance_id)
            .and_then(|p| p.models.iter().find(|m| m.slug == current.model))
            .and_then(|m| m.capabilities.as_ref())
            .and_then(|c| serde_json::to_value(c).ok())
            .and_then(|v| v.get("optionDescriptors").cloned())
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
    }

    fn option_value(&self, descriptor: &Value) -> Option<Value> {
        let id = descriptor.get("id")?.as_str()?;
        self.model
            .as_ref()
            .and_then(|m| m.options.iter().find(|(o, _)| o == id).map(|(_, v)| v.clone()))
            .or_else(|| descriptor.get("currentValue").cloned())
            .or_else(|| {
                descriptor
                    .get("options")?
                    .as_array()?
                    .iter()
                    .find(|o| o.get("isDefault").and_then(Value::as_bool) == Some(true))
                    .and_then(|o| o.get("id").cloned())
            })
    }

    /// "Medium · 1M": each option's current label (`buildTraitsTriggerDisplay`).
    fn traits_label(&self, cx: &App) -> Option<SharedString> {
        let mut labels = Vec::new();
        let mut fast: Option<bool> = None;
        let codex = self.model.as_ref().is_some_and(|m| m.instance_id.to_ascii_lowercase().contains("codex"));
        for descriptor in self.descriptors(cx) {
            let id = descriptor.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let value = self.option_value(&descriptor);
            // Codex's service tier reads as fast mode when it has a "Fast" tier.
            if codex && id == "serviceTier" {
                let options = descriptor.get("options").and_then(Value::as_array).cloned().unwrap_or_default();
                let fast_tier = options
                    .iter()
                    .find(|o| o.get("label").and_then(Value::as_str) == Some("Fast"))
                    .and_then(|o| o.get("id").and_then(Value::as_str))
                    .map(String::from);
                let current = value.as_ref().and_then(Value::as_str).unwrap_or_default().to_owned();
                if let Some(tier) = fast_tier.filter(|t| current == "default" || current == *t) {
                    fast = Some(current == tier);
                    continue;
                }
            }
            match descriptor.get("type").and_then(Value::as_str) {
                Some("boolean") if id == "fastMode" => fast = Some(value.and_then(|v| v.as_bool()).unwrap_or(false)),
                Some("boolean") => {
                    let label = descriptor.get("label").and_then(Value::as_str).unwrap_or(&id).to_owned();
                    labels.push(format!("{label} {}", if value.and_then(|v| v.as_bool()) == Some(true) { "On" } else { "Off" }));
                }
                _ => {
                    let selected = value.as_ref().and_then(Value::as_str).unwrap_or_default().to_owned();
                    if let Some(label) = descriptor
                        .get("options")
                        .and_then(Value::as_array)
                        .and_then(|o| o.iter().find(|o| o.get("id").and_then(Value::as_str) == Some(selected.as_str())))
                        .and_then(|o| o.get("label").and_then(Value::as_str))
                    {
                        labels.push(label.to_owned());
                    }
                }
            }
        }
        if labels.is_empty() {
            return fast.map(|on| SharedString::from(if on { "Fast" } else { "Normal" }));
        }
        Some(labels.join(" · ").into())
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
        if entries.is_empty() {
            entries.push(Entry::item("No provider is ready (see Settings)", |_, _| {}).disabled(true));
        }
        self.menu = Some(OpenMenu::new(entries, event.position, window, cx, |this, _, _| this.menu = None).upward());
        cx.notify();
    }

    fn open_traits_menu(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let mut entries: Vec<Entry> = Vec::new();
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
        if entries.first().is_some_and(|e| matches!(e, Entry::Separator)) {
            entries.remove(0);
        }
        if entries.is_empty() {
            return;
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
                let glyph = runtime_mode_icon(mode);
                let mode = mode.to_string();
                let checked = self.runtime_mode == mode;
                Entry::item(*label, move |_, cx| {
                    let _ = this.update(cx, |c, cx| {
                        c.runtime_mode = mode.clone();
                        cx.notify();
                    });
                })
                .icon(glyph)
                .detail(*detail)
                .checked(checked)
            })
            .collect::<Vec<_>>();
        // Build or Plan, as the web's compact controls menu offers them.
        let mut entries = entries;
        entries.insert(0, Entry::Header("Access".into()));
        entries.push(Entry::Separator);
        entries.push(Entry::Header("Mode".into()));
        for (plan, label, glyph) in [(false, "Build", Icon::Bot), (true, "Plan", Icon::PencilRuler)] {
            let this = this.clone();
            entries.push(
                Entry::item(label, move |_, cx| {
                    let _ = this.update(cx, |c, cx| {
                        c.plan_mode = plan;
                        cx.notify();
                    });
                })
                .icon(glyph)
                .checked(self.plan_mode == plan),
            );
        }
        self.menu = Some(OpenMenu::new(entries, event.position, window, cx, |this, _, _| this.menu = None).upward());
        cx.notify();
    }

    fn open_env_menu(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let entries = [
            (EnvMode::Local, "Current checkout", "Works in the project's folder"),
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

/// A banner over the composer: its icon (a dot when none), title, description and action.
struct Banner {
    glyph: Option<Icon>,
    title: &'static str,
    description: Option<&'static str>,
    action: &'static str,
    /// The registry command of the action; none stops the background work.
    command: Option<&'static str>,
}

/// A control of the composer's footer (`ComposerControl size="sm"`): 28 px, 10 px across, a
/// 16 px icon, 14 px medium muted text, the 14 px chevron in the icon color.
fn control(id: &'static str, glyph: Option<(Icon, Option<gpui::Hsla>)>, label: SharedString, cx: &App) -> gpui::Stateful<gpui::Div> {
    let c = cx.theme().colors.clone();
    let (accent, text) = (c.accent_soft, c.text);
    div()
        .id(id)
        .group(id)
        .h(px(28.))
        .px(px(10.))
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.))
        .rounded(px(radius::MD))
        .cursor_pointer()
        .text_size(px(14.))
        .line_height(px(20.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(c.text_2)
        .hover(move |s| s.bg(accent).text_color(text))
        .when_some(glyph, |el, (glyph, color)| {
            el.child(svg().path(glyph.path()).size(px(16.)).flex_none().text_color(color.unwrap_or(c.text_2)))
        })
        .child(div().max_w(px(240.)).one_line().child(label))
        .child(svg().path(Icon::ChevronDownThick.path()).size(px(14.)).flex_none().text_color(c.text_3))
}

/// The 1×16 line between footer controls.
fn control_separator(cx: &App) -> gpui::Div {
    div().w(px(1.)).h(px(16.)).mx(px(2.)).flex_none().bg(cx.theme().colors.line)
}

/// An `xs` control of the strip under the composer: 24 px, 12 px text at 70% of the muted
/// color, a 12 px icon.
fn strip_control(id: &'static str, glyph: Icon, label: SharedString, chevron: bool, cx: &App) -> gpui::Stateful<gpui::Div> {
    let c = cx.theme().colors.clone();
    let muted = c.text_2.opacity(0.7);
    let accent = c.accent_soft;
    div()
        .id(id)
        .h(px(24.))
        .px(px(7.))
        .flex()
        .flex_none()
        .items_center()
        .gap(px(4.))
        .rounded(px(radius::MD))
        .text_size(px(12.))
        .line_height(px(16.))
        .text_color(muted)
        .when(chevron, |el| el.cursor_pointer().hover(move |s| s.bg(accent)))
        .child(svg().path(glyph.path()).size(px(12.)).flex_none().text_color(muted))
        .child(div().max_w(px(240.)).one_line().child(label))
        .when(chevron, |el| {
            el.child(
                svg()
                    .path(Icon::ChevronDown.path())
                    .size(px(12.))
                    .flex_none()
                    .text_color(c.text_2.opacity(0.35)),
            )
        })
}

/// The glass of the composer and its drawers, as one color: GPUI cannot blur, and at rest
/// the web's glass only covers the work's background (measured on the web's screenshots).
pub fn composer_surface(cx: &App) -> (gpui::Hsla, gpui::Hsla, gpui::Hsla) {
    let dark = cx.theme().mode == crate::theme::Mode::Dark;
    let c = crate::theme::parse_color;
    // (composer, drawer, edge)
    if dark {
        (c("#1a1c23"), c("#1c1e25"), c("#2e3036"))
    } else {
        (c("#fdfdff"), c("#fcfcfe"), c("#ffffff"))
    }
}

impl Render for Composer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let c = theme.colors.clone();
        let dark = theme.mode == crate::theme::Mode::Dark;
        let (surface, drawer, edge) = composer_surface(cx);
        let empty = self.editor.read(cx).text().trim().is_empty();
        let model_label = self.model_label(cx);
        let traits = self.traits_label(cx);
        let provider = self.model.as_ref().map(|m| m.instance_id.clone()).unwrap_or_default();
        let (provider_glyph, provider_color) = crate::ui::badges::provider_logo(&provider);
        let context = self.context.clone();
        let mode = self.runtime_mode.clone();
        let accent = c.accent_soft;
        let shadow = vec![BoxShadow {
            color: gpui::hsla(0., 0., 0., 0.4),
            offset: gpui::point(px(0.), px(12.)),
            blur_radius: px(28.),
            spread_radius: px(-18.),
        }];

        // The banner over the composer, 22 px in on each side, tucked under it
        // (`ComposerBannerStack`): "Monitoring", or a settled or snoozed thread.
        let banner = if context.monitoring && !self.running {
            Some(Banner {
                glyph: None,
                title: "Monitoring",
                description: None,
                action: "Stop",
                command: None,
            })
        } else if context.settled {
            Some(Banner {
                glyph: Some(Icon::CircleCheck),
                title: "This thread is settled",
                description: Some("Send a message to unsettle"),
                action: "Un-settle",
                command: Some("thread.reopen"),
            })
        } else if context.snoozed {
            Some(Banner {
                glyph: Some(Icon::AlarmClock),
                title: "This thread is snoozed",
                description: Some("Send a message to wake"),
                action: "Wake",
                command: Some("thread.wake"),
            })
        } else {
            None
        };
        let banner = banner.map(
            |Banner {
                 glyph,
                 title,
                 description,
                 action,
                 command,
             }| {
                div()
                    // 34 px show over the composer: 5 px, the 24 px row, 4 px (measured on the web).
                    .mx(px(22.))
                    .h(px(34.))
                    .pt(px(5.))
                    .px(px(4.))
                    .rounded_t(px(radius::XXL))
                    .border_t_1()
                    .border_l_1()
                    .border_r_1()
                    .border_color(edge)
                    .bg(drawer)
                    .child(
                        div()
                            .h(px(24.))
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .text_size(px(12.))
                            .line_height(px(16.))
                            .child(div().w(px(24.)).flex().justify_center().child(match glyph {
                                Some(glyph) => svg().path(glyph.path()).size(px(12.)).text_color(c.text_2).into_any_element(),
                                None => div().size(px(6.)).rounded_full().bg(c.text).into_any_element(),
                            }))
                            .child(
                                div()
                                    .flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap(px(4.))
                                    .child(div().flex_none().font_weight(FontWeight::MEDIUM).text_color(c.text).child(title))
                                    .children(description.map(|d| div().min_w_0().one_line().text_color(c.text_2).child(d))),
                            )
                            .child(
                                div()
                                    .id("banner-action")
                                    .h(px(24.))
                                    .px(px(7.))
                                    .flex()
                                    .items_center()
                                    .rounded(px(radius::MD))
                                    .cursor_pointer()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(c.text)
                                    .hover(move |s| s.bg(accent))
                                    .child(action)
                                    .on_click(cx.listener(move |_, _, _, cx| match command {
                                        Some(command) => cx.emit(ComposerEvent::Command(command)),
                                        None => cx.emit(ComposerEvent::Stop),
                                    })),
                            ),
                    )
            },
        );

        let images = (!self.images.is_empty()).then(|| {
            div()
                .mb(px(12.))
                .flex()
                .flex_wrap()
                .gap(px(8.))
                .children(self.images.iter().enumerate().map(|(i, path)| {
                    div()
                        .relative()
                        .size(px(64.))
                        .rounded(px(radius::LG))
                        .border_1()
                        .border_color(c.line.opacity(c.line.a * 0.8))
                        .bg(c.bg_raised)
                        .overflow_hidden()
                        .child(gpui::img(path.clone()).size_full().object_fit(gpui::ObjectFit::Cover))
                        .child(
                            div()
                                .id(SharedString::from(format!("unattach-{i}")))
                                .absolute()
                                .top(px(4.))
                                .right(px(4.))
                                .size(px(24.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(radius::MD))
                                .bg(gpui::hsla(0., 0., 0., 0.65))
                                .cursor_pointer()
                                .child(svg().path(Icon::X.path()).size(px(14.)).text_color(gpui::white()))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if i < this.images.len() {
                                        this.images.remove(i);
                                    }
                                    cx.notify();
                                })),
                        )
                }))
        });

        let send = div()
            .id("send")
            .size(px(32.))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(c.accent_fill)
            .when(empty || self.disabled_reason.is_some(), |el| el.opacity(0.64))
            .when(!empty && self.disabled_reason.is_none(), |el| {
                el.cursor_pointer()
                    .hover(|s| s.bg(c.accent_hover))
                    .on_click(cx.listener(|this, _, _, cx| this.send(cx)))
            })
            .child(svg().path(Icon::SendArrow.path()).size(px(14.)).text_color(c.text_on_accent));
        let stop = div()
            .id("stop")
            .size(px(32.))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(c.danger.opacity(0.9))
            .cursor_pointer()
            .hover(|s| s.bg(c.danger))
            .child(svg().path(Icon::StopSquare.path()).size(px(12.)).text_color(gpui::white()))
            .on_click(cx.listener(|_, _, _, cx| cx.emit(ComposerEvent::Stop)));

        let composer = div()
            .relative()
            .w_full()
            .flex()
            .flex_col()
            .rounded(px(radius::XXXL))
            .bg(surface)
            .border_1()
            .border_color(edge)
            .when(!dark, |el| el.shadow(shadow.clone()))
            .child(
                div()
                    .px(px(16.))
                    .pt(px(16.))
                    .pb(px(8.))
                    .children(images)
                    .child(div().min_h(px(70.)).child(self.editor.clone()))
                    .when_some(self.disabled_reason.clone(), |el, reason| {
                        el.child(div().pt(px(4.)).text_size(px(12.)).text_color(c.warning_text).child(reason))
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(16.))
                    .pb(px(16.))
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .items_center()
                            .gap(px(4.))
                            .ml(px(-10.))
                            .child(
                                control("model", Some((provider_glyph, provider_color)), model_label, cx)
                                    .on_mouse_down(MouseButton::Left, cx.listener(Self::open_model_menu)),
                            )
                            .when_some(traits, |el, traits| {
                                el.child(control_separator(cx))
                                    .child(control("traits", None, traits, cx).on_mouse_down(MouseButton::Left, cx.listener(Self::open_traits_menu)))
                            })
                            .child(control_separator(cx))
                            .child(
                                control("runtime-mode", Some((runtime_mode_icon(&mode), None)), runtime_mode_label(&mode).into(), cx)
                                    .on_mouse_down(MouseButton::Left, cx.listener(Self::open_mode_menu)),
                            )
                            .when(self.plan_mode, |el| {
                                el.child(control_separator(cx))
                                    .child(
                                        control("plan-mode", Some((Icon::PencilRuler, None)), "Plan".into(), cx).on_click(cx.listener(|this, _, _, cx| {
                                            this.plan_mode = false;
                                            cx.notify();
                                        })),
                                    )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .id("attach")
                                    .size(px(28.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(radius::MD))
                                    .cursor_pointer()
                                    .hover(move |s| s.bg(accent))
                                    .tooltip(|_, cx| crate::ui::Tooltip::view("Attach files", None, cx))
                                    .child(svg().path(Icon::Paperclip.path()).size(px(16.)).text_color(c.text_2))
                                    .on_click(cx.listener(|this, _, _, cx| this.attach_images(cx))),
                            )
                            .when(self.running, |el| el.child(stop))
                            .when(!self.running || !empty, |el| el.child(send)),
                    ),
            );

        // The strip under the composer: where the thread works, its pull request, its branch.
        let workspace: (Icon, SharedString) = match (context.draft, self.env_mode, context.worktree) {
            (true, Some(EnvMode::Worktree), _) => (Icon::FolderGit2, "New worktree".into()),
            (true, _, _) => (Icon::Folder, "Current checkout".into()),
            (false, _, true) => (Icon::FolderGit2, "Worktree".into()),
            (false, _, false) => (Icon::Folder, "Local checkout".into()),
        };
        let strip = div()
            .mx(px(22.))
            .h(px(32.))
            .pt(px(4.))
            .pb(px(4.))
            .pl(px(4.))
            .pr(px(8.))
            .flex()
            .items_center()
            .gap(px(4.))
            .rounded_b(px(radius::XXL))
            .border_b_1()
            .border_l_1()
            .border_r_1()
            .border_color(edge)
            .bg(drawer)
            .child(
                strip_control("workspace", workspace.0, workspace.1, context.draft, cx)
                    .when(context.draft, |el| el.on_mouse_down(MouseButton::Left, cx.listener(Self::open_env_menu))),
            )
            .child(div().flex_1())
            .when_some(context.pull_request.clone(), |el, badge| {
                let url = badge.url.clone();
                el.child(
                    div()
                        .id("strip-pr")
                        .h(px(24.))
                        .px(px(7.))
                        .flex()
                        .items_center()
                        .rounded(px(radius::MD))
                        .cursor_pointer()
                        .on_click(move |_, _, cx| cx.open_url(&url))
                        .child(crate::ui::badges::pull_request_badge(&badge, cx)),
                )
            })
            .when_some(context.branch.clone(), |el, branch| {
                el.child(strip_control("branch", Icon::GitBranch, branch.into(), true, cx))
            });

        div()
            .flex()
            .flex_col()
            .w_full()
            .children(banner)
            .child(composer)
            .child(strip)
            .when_some(self.menu.as_ref(), |this, menu| this.child(menu.render()))
    }
}
